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
    // Whether faces have been detected for this photo (0 = not yet).
    let _ = conn.execute(
        "ALTER TABLE photos ADD COLUMN faces_scanned INTEGER NOT NULL DEFAULT 0",
        [],
    );
    // The timeline orders on COALESCE(taken_ts, mtime); index it for fast paging.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_photos_sort ON photos(COALESCE(taken_ts, mtime));
         CREATE TABLE IF NOT EXISTS faces (
            id         INTEGER PRIMARY KEY,
            photo_id   INTEGER NOT NULL,
            x1         REAL NOT NULL,
            y1         REAL NOT NULL,
            x2         REAL NOT NULL,
            y2         REAL NOT NULL,
            score      REAL NOT NULL,
            embedding  BLOB NOT NULL,
            cluster_id INTEGER
         );
         CREATE INDEX IF NOT EXISTS idx_faces_photo ON faces(photo_id);
         CREATE INDEX IF NOT EXISTS idx_faces_cluster ON faces(cluster_id);
         CREATE TABLE IF NOT EXISTS cluster_names (
            cluster_id INTEGER PRIMARY KEY,
            name       TEXT NOT NULL
         );",
    )?;
    Ok(())
}

/// One detected face ready to persist: bounding box, detector score, and the
/// L2-normalized embedding.
pub struct DetectedFace {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    pub score: f32,
    pub embedding: Vec<f32>,
}

/// Photos that have a thumbnail (so the original is available locally) but whose
/// faces haven't been detected yet. This naturally limits face work to files we
/// already have — cloud-only photos are processed after they're downloaded.
pub fn next_unscanned_for_faces(conn: &Connection, limit: i64) -> Result<Vec<Job>> {
    let mut stmt = conn.prepare(
        "SELECT id, path FROM photos WHERE thumb_status = ?1 AND faces_scanned = 0 ORDER BY id LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![STATUS_READY, limit], |r| {
        Ok(Job { id: r.get(0)?, path: r.get(1)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Persist a photo's detected faces (each with its assigned cluster id) and mark
/// the photo scanned (one transaction).
pub fn save_faces(
    conn: &mut Connection,
    photo_id: i64,
    faces: &[DetectedFace],
    cluster_ids: &[i64],
) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut ins = tx.prepare(
            "INSERT INTO faces (photo_id, x1, y1, x2, y2, score, embedding, cluster_id) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        )?;
        for (f, cid) in faces.iter().zip(cluster_ids) {
            let bytes: Vec<u8> = f.embedding.iter().flat_map(|v| v.to_le_bytes()).collect();
            ins.execute(rusqlite::params![photo_id, f.x1, f.y1, f.x2, f.y2, f.score, bytes, cid])?;
        }
        tx.execute("UPDATE photos SET faces_scanned = 1 WHERE id = ?1", [photo_id])?;
    }
    tx.commit()?;
    Ok(())
}

/// (cluster_id, embedding) for every already-clustered face — used to rebuild the
/// in-memory cluster index at startup.
pub fn clustered_embeddings(conn: &Connection) -> Result<Vec<(i64, Vec<f32>)>> {
    let mut stmt =
        conn.prepare("SELECT cluster_id, embedding FROM faces WHERE cluster_id IS NOT NULL")?;
    let rows = stmt.query_map([], |r| {
        let cid: i64 = r.get(0)?;
        let blob: Vec<u8> = r.get(1)?;
        let emb: Vec<f32> = blob
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        Ok((cid, emb))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One person's group: cluster id, face count, a cover face (highest-confidence
/// detection), and a name if it's been named. Biggest groups first.
#[derive(serde::Serialize)]
pub struct ClusterRow {
    pub cluster_id: i64,
    pub count: i64,
    pub cover_face_id: i64,
    pub name: Option<String>,
}

pub fn clusters_overview(conn: &Connection) -> Result<Vec<ClusterRow>> {
    let mut stmt = conn.prepare(
        "SELECT f.cluster_id, COUNT(*) AS c,
                (SELECT id FROM faces f2 WHERE f2.cluster_id = f.cluster_id ORDER BY score DESC LIMIT 1),
                (SELECT name FROM cluster_names cn WHERE cn.cluster_id = f.cluster_id)
         FROM faces f
         WHERE f.cluster_id IS NOT NULL
         GROUP BY f.cluster_id
         ORDER BY c DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ClusterRow {
            cluster_id: r.get(0)?,
            count: r.get(1)?,
            cover_face_id: r.get(2)?,
            name: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Name (or rename) a cluster. Empty name clears it.
pub fn name_cluster(conn: &Connection, cluster_id: i64, name: &str) -> Result<()> {
    if name.trim().is_empty() {
        conn.execute("DELETE FROM cluster_names WHERE cluster_id = ?1", [cluster_id])?;
    } else {
        conn.execute(
            "INSERT INTO cluster_names (cluster_id, name) VALUES (?1, ?2)
             ON CONFLICT(cluster_id) DO UPDATE SET name = excluded.name",
            rusqlite::params![cluster_id, name.trim()],
        )?;
    }
    Ok(())
}

/// Merge cluster `from` into `into`: reassign its faces and drop its name.
/// The surviving cluster keeps `into`'s name.
pub fn merge_clusters(conn: &Connection, into: i64, from: i64) -> Result<()> {
    conn.execute(
        "UPDATE faces SET cluster_id = ?1 WHERE cluster_id = ?2",
        rusqlite::params![into, from],
    )?;
    conn.execute("DELETE FROM cluster_names WHERE cluster_id = ?1", [from])?;
    Ok(())
}

/// The photo id and (normalized 0-1) bounding box of a face, for cropping.
pub fn face_box(conn: &Connection, face_id: i64) -> Result<Option<(i64, f32, f32, f32, f32)>> {
    let row = conn
        .query_row(
            "SELECT photo_id, x1, y1, x2, y2 FROM faces WHERE id = ?1",
            [face_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .ok();
    Ok(row)
}

/// Delete the faces belonging to a set of photos (call when photos are removed
/// so we don't leave orphaned faces behind).
pub fn delete_faces_for_photos(conn: &Connection, photo_ids: &[i64]) -> Result<()> {
    if photo_ids.is_empty() {
        return Ok(());
    }
    let placeholders = std::iter::repeat("?").take(photo_ids.len()).collect::<Vec<_>>().join(",");
    let sql = format!("DELETE FROM faces WHERE photo_id IN ({placeholders})");
    conn.execute(&sql, rusqlite::params_from_iter(photo_ids.iter()))?;
    Ok(())
}

/// (photos scanned for faces, photos eligible) — drives the "Finding people…"
/// progress readout. Eligible = has a thumbnail (is local).
pub fn face_progress(conn: &Connection) -> Result<(i64, i64)> {
    let eligible: i64 = conn.query_row(
        "SELECT COUNT(*) FROM photos WHERE thumb_status = ?1",
        [STATUS_READY],
        |r| r.get(0),
    )?;
    let scanned: i64 = conn.query_row(
        "SELECT COUNT(*) FROM photos WHERE thumb_status = ?1 AND faces_scanned = 1",
        [STATUS_READY],
        |r| r.get(0),
    )?;
    Ok((scanned, eligible))
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

/// Photos missing a capture date (id, path) — used to backfill filename dates
/// into a library that was indexed before that fallback existed.
pub fn null_date_photos(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare("SELECT id, path FROM photos WHERE taken_ts IS NULL")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Set capture dates for many photos in one transaction.
pub fn set_taken_ts_batch(conn: &mut Connection, pairs: &[(i64, i64)]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut up = tx.prepare("UPDATE photos SET taken_ts = ?1 WHERE id = ?2 AND taken_ts IS NULL")?;
        for (id, ts) in pairs {
            up.execute(rusqlite::params![ts, id])?;
        }
    }
    tx.commit()?;
    Ok(())
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
