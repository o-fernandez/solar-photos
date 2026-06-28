//! SQLite is the source of truth for *what photos exist* and *whether each
//! thumbnail is cached yet*. The actual thumbnail pixels live as files on disk
//! (see `thumbs.rs`); the DB only tracks status. This split is what makes a
//! repeat launch instant (Principle 4): we read the photo list straight from
//! the DB and the already-generated thumbnails straight from disk — no rescan,
//! no re-decode.

use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

/// Thumbnail status values stored in the `photos.thumb_status` column.
pub const STATUS_PENDING: i64 = 0; // local file, queued for thumbnailing
pub const STATUS_READY: i64 = 1; // thumbnail cached on disk
pub const STATUS_FAILED: i64 = 2; // local file we couldn't decode
/// Cloud-only original (not downloaded locally). We do NOT thumbnail these up
/// front — that would mean bulk-downloading the whole library. They wait as
/// placeholders until the user scrolls to them (see on-demand handling in
/// `lib.rs`), at which point they move to DOWNLOADING.
pub const STATUS_CLOUD: i64 = 3;
/// A cloud-only original the user has scrolled to; we're fetching + thumbnailing
/// it now. On success it becomes READY; on failure it falls back to CLOUD so a
/// later visit can retry.
pub const STATUS_DOWNLOADING: i64 = 4;

/// Open (or create) the library database.
///
/// WAL mode lets readers (the grid querying rows) run concurrently with writers
/// (the scan inserting photos, workers updating status). `busy_timeout` makes the
/// several writers (scan + local workers + cloud workers) wait politely for the
/// single write lock instead of erroring out.
pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    Ok(conn)
}

/// Create the schema if it does not exist yet.
pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS photos (
            id           INTEGER PRIMARY KEY,
            path         TEXT UNIQUE NOT NULL,
            mtime        INTEGER NOT NULL,
            size         INTEGER NOT NULL,
            cache_key    TEXT NOT NULL,
            thumb_status INTEGER NOT NULL DEFAULT 0,
            taken_ts     INTEGER,
            seen         INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS idx_photos_path ON photos(path);
         CREATE INDEX IF NOT EXISTS idx_photos_status ON photos(thumb_status);
         CREATE TABLE IF NOT EXISTS roots (
            id   INTEGER PRIMARY KEY,
            path TEXT UNIQUE NOT NULL
         );",
    )?;
    // Migrations for libraries created before these columns existed. Ignore the
    // error if the column is already present.
    let _ = conn.execute("ALTER TABLE photos ADD COLUMN taken_ts INTEGER", []);
    let _ = conn.execute("ALTER TABLE photos ADD COLUMN seen INTEGER NOT NULL DEFAULT 0", []);
    // The timeline orders on COALESCE(taken_ts, mtime); index it for fast paging.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_photos_sort ON photos(COALESCE(taken_ts, mtime));",
    )?;
    Ok(())
}

/// Remember a folder the user added. Idempotent.
pub fn add_root(conn: &Connection, path: &str) -> Result<()> {
    conn.execute("INSERT OR IGNORE INTO roots (path) VALUES (?1)", [path])?;
    Ok(())
}

/// The folders the library is built from.
pub fn list_roots(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT path FROM roots ORDER BY path")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Forget a root and return the ids of the photos it contained (so the caller
/// can delete their cached files). The photo rows are deleted here.
pub fn remove_root(conn: &Connection, path: &str) -> Result<Vec<i64>> {
    conn.execute("DELETE FROM roots WHERE path = ?1", [path])?;
    let pattern = format!("{path}/%");
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM photos WHERE path = ?1 OR path LIKE ?2")?;
        let rows = stmt.query_map(rusqlite::params![path, pattern], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    conn.execute(
        "DELETE FROM photos WHERE path = ?1 OR path LIKE ?2",
        rusqlite::params![path, pattern],
    )?;
    Ok(ids)
}

/// After a full rescan, delete every photo not marked with the current
/// generation (its file is gone, or its root was removed). Returns the deleted
/// ids so their cached files can be removed too.
pub fn take_unseen(conn: &Connection, gen: i64) -> Result<Vec<i64>> {
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM photos WHERE seen <> ?1")?;
        let rows = stmt.query_map([gen], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    conn.execute("DELETE FROM photos WHERE seen <> ?1", [gen])?;
    Ok(ids)
}

/// A unit of work for the thumbnail pipeline: which photo, and where its
/// original lives. The cached thumbnail's location is derived from `id` alone
/// (see `thumbs::thumb_path`), so a job needs nothing more.
#[derive(Clone, Debug)]
pub struct Job {
    pub id: i64,
    pub path: String,
}

/// Local photos that still need a thumbnail (status = pending). Used at startup
/// to resume an interrupted indexing run. Cloud-only photos are intentionally
/// excluded — they wait for the user to look at them.
pub fn pending_jobs(conn: &Connection) -> Result<Vec<Job>> {
    let mut stmt =
        conn.prepare("SELECT id, path FROM photos WHERE thumb_status = ?1 ORDER BY id")?;
    let rows = stmt.query_map([STATUS_PENDING], |r| {
        Ok(Job {
            id: r.get(0)?,
            path: r.get(1)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// (total photos, thumbnails ready) — drives the header progress readout and
/// lets a cold start render the grid immediately from cached state.
pub fn stats(conn: &Connection) -> Result<(i64, i64)> {
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))?;
    let ready: i64 = conn.query_row(
        "SELECT COUNT(*) FROM photos WHERE thumb_status = ?1",
        [STATUS_READY],
        |r| r.get(0),
    )?;
    Ok((total, ready))
}

/// One grid cell's worth of data: a stable id, its thumbnail status, and the
/// timestamp it sorts/labels by (capture date if known, else file mtime).
#[derive(serde::Serialize)]
pub struct PhotoRow {
    pub id: i64,
    pub status: i64,
    pub ts: i64,
}

/// Fetch a contiguous window of the library.
///
/// Two orderings:
///   * "discovery" (by id) — used *during* a live scan, so newly found photos
///     only ever append to the end and the view never reshuffles (Principle 2).
///   * "date" — newest-first by capture date (mtime fallback). Used once the
///     scan finishes (the "snap to timeline" moment) and on every cold start.
pub fn photos_range(conn: &Connection, offset: i64, limit: i64, by_date: bool) -> Result<Vec<PhotoRow>> {
    let sql = if by_date {
        "SELECT id, thumb_status, COALESCE(taken_ts, mtime) AS ts FROM photos
         ORDER BY ts DESC, id DESC LIMIT ?1 OFFSET ?2"
    } else {
        "SELECT id, thumb_status, COALESCE(taken_ts, mtime) AS ts FROM photos
         ORDER BY id LIMIT ?1 OFFSET ?2"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([limit, offset], |r| {
        Ok(PhotoRow {
            id: r.get(0)?,
            status: r.get(1)?,
            ts: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Record a photo's capture date once we've read it from a now-local file.
/// Only fills it when still empty, so we never clobber a date set at scan time.
pub fn set_taken_ts_if_empty(conn: &Connection, id: i64, ts: i64) -> Result<()> {
    conn.execute(
        "UPDATE photos SET taken_ts = ?1 WHERE id = ?2 AND taken_ts IS NULL",
        rusqlite::params![ts, id],
    )?;
    Ok(())
}

/// Look up status + path for a set of ids. Used by on-demand cloud handling to
/// decide which of the currently-visible photos are cloud-only and need fetching.
pub fn lookup(conn: &Connection, ids: &[i64]) -> Result<Vec<(i64, i64, String)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat("?").take(ids.len()).collect::<Vec<_>>().join(",");
    let sql = format!("SELECT id, thumb_status, path FROM photos WHERE id IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let params = rusqlite::params_from_iter(ids.iter());
    let rows = stmt.query_map(params, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The original file path for one photo id (used by the preview protocol).
pub fn path_for_id(conn: &Connection, id: i64) -> Result<Option<String>> {
    let path = conn
        .query_row("SELECT path FROM photos WHERE id = ?1", [id], |r| r.get(0))
        .ok();
    Ok(path)
}

/// Detail shown in the viewer chrome: (full path, timestamp). The timestamp is
/// the capture date once we have it, else the file's modified-time.
pub fn detail(conn: &Connection, id: i64) -> Result<Option<(String, i64)>> {
    let row = conn
        .query_row(
            "SELECT path, COALESCE(taken_ts, mtime) FROM photos WHERE id = ?1",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .ok();
    Ok(row)
}

/// Mark a thumbnail's outcome (ready / failed / downloading / cloud).
pub fn set_status(conn: &Connection, id: i64, status: i64) -> Result<()> {
    conn.execute(
        "UPDATE photos SET thumb_status = ?1 WHERE id = ?2",
        rusqlite::params![status, id],
    )?;
    Ok(())
}

/// Move a set of ids to a status in one statement (used when we start fetching
/// the cloud-only photos the user just scrolled to).
pub fn set_status_many(conn: &Connection, ids: &[i64], status: i64) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders = std::iter::repeat("?").take(ids.len()).collect::<Vec<_>>().join(",");
    let sql = format!("UPDATE photos SET thumb_status = {status} WHERE id IN ({placeholders})");
    let params = rusqlite::params_from_iter(ids.iter());
    conn.execute(&sql, params)?;
    Ok(())
}

/// At startup, any photo left in DOWNLOADING wasn't actually being fetched (the
/// previous run ended). Reset it to CLOUD so it shows as a placeholder and gets
/// re-fetched the next time it's visible.
pub fn set_status_many_where_downloading(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE photos SET thumb_status = ?1 WHERE thumb_status = ?2",
        rusqlite::params![STATUS_CLOUD, STATUS_DOWNLOADING],
    )?;
    Ok(())
}
