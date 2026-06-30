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
         );
         CREATE TABLE IF NOT EXISTS app_meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
         );",
    )?;
    // Durable person records. Unlike cluster ids — which are reassigned from
    // scratch on every re-cluster — an identity id is permanent, so it can carry
    // the user's decisions (this is so-and-so; these groups are the same person)
    // across re-clusters. A face's `identity_id` is the must-link: every face
    // sharing one is forced into a single cluster no matter what the embeddings do.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS identities (
            id   INTEGER PRIMARY KEY,
            name TEXT
         );
         CREATE TABLE IF NOT EXISTS cannot_link (
            a INTEGER NOT NULL,
            b INTEGER NOT NULL,
            PRIMARY KEY (a, b)
         );",
    )?;
    // Migration for libraries created before identities existed.
    let _ = conn.execute("ALTER TABLE faces ADD COLUMN identity_id INTEGER", []);
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_faces_identity ON faces(identity_id);")?;
    // `ignored` faces are excluded from People for good (a stranger, a poster, a
    // reflection) — they keep cluster_id = NULL like a detach, but the flag marks
    // the exclusion as intentional and permanent so the overlay never re-draws them.
    let _ = conn.execute("ALTER TABLE faces ADD COLUMN ignored INTEGER NOT NULL DEFAULT 0", []);
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

/// `faces_scanned` sentinel: claimed by a worker, detection in flight. Sits
/// between 0 (unscanned) and 1 (done) so concurrent workers never grab the same
/// photo, and so `claim_faces_batch` / `face_progress` ignore it. Any rows
/// left at this value after a crash are reset to 0 at startup.
pub const FACES_CLAIMED: i64 = 2;

/// Atomically take up to `limit` unscanned photos and mark them in-flight, so a
/// pool of face workers can run in parallel without processing the same photo
/// twice. Returns the claimed jobs (already flipped to FACES_CLAIMED).
pub fn claim_faces_batch(conn: &mut Connection, limit: i64) -> Result<Vec<Job>> {
    let tx = conn.transaction()?;
    let jobs: Vec<Job> = {
        let mut stmt = tx.prepare(
            "SELECT id, path FROM photos WHERE thumb_status = ?1 AND faces_scanned = 0 ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![STATUS_READY, limit], |r| {
            Ok(Job { id: r.get(0)?, path: r.get(1)? })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for j in &jobs {
        tx.execute(
            "UPDATE photos SET faces_scanned = ?1 WHERE id = ?2",
            rusqlite::params![FACES_CLAIMED, j.id],
        )?;
    }
    tx.commit()?;
    Ok(jobs)
}

/// Clear any in-flight claims left behind by a crash/quit, so those photos are
/// scanned again on the next run. Returns how many rows were reset.
pub fn reset_claimed_faces(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE photos SET faces_scanned = 0 WHERE faces_scanned = ?1",
        [FACES_CLAIMED],
    )?)
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

/// Decode a stored little-endian f32 embedding blob back into a vector.
fn decode_embedding(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// (cluster_id, embedding) for every already-clustered face — used to rebuild the
/// in-memory cluster index at startup.
pub fn clustered_embeddings(conn: &Connection) -> Result<Vec<(i64, Vec<f32>)>> {
    let mut stmt =
        conn.prepare("SELECT cluster_id, embedding FROM faces WHERE cluster_id IS NOT NULL")?;
    let rows = stmt.query_map([], |r| {
        let cid: i64 = r.get(0)?;
        let blob: Vec<u8> = r.get(1)?;
        Ok((cid, decode_embedding(&blob)))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// (face_id, embedding) for every face that still belongs to a cluster — the input
/// to a full [`crate::cluster::recluster`]. We deliberately skip NULL-cluster faces:
/// those were detached by "not this person" and must stay out (a re-cluster must not
/// resurrect them).
pub fn all_face_embeddings(conn: &Connection) -> Result<Vec<(i64, Vec<f32>)>> {
    let mut stmt =
        conn.prepare("SELECT id, embedding FROM faces WHERE cluster_id IS NOT NULL")?;
    let rows = stmt.query_map([], |r| {
        let id: i64 = r.get(0)?;
        let blob: Vec<u8> = r.get(1)?;
        Ok((id, decode_embedding(&blob)))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// (face_id, cluster_id, embedding) for every clustered face — the input to
/// face-to-face merge-suggestion evidence.
pub fn face_cluster_embeddings(conn: &Connection) -> Result<Vec<(i64, i64, Vec<f32>)>> {
    let mut stmt =
        conn.prepare("SELECT id, cluster_id, embedding FROM faces WHERE cluster_id IS NOT NULL")?;
    let rows = stmt.query_map([], |r| {
        let id: i64 = r.get(0)?;
        let cid: i64 = r.get(1)?;
        let blob: Vec<u8> = r.get(2)?;
        Ok((id, cid, decode_embedding(&blob)))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// (face_id, cluster_id) for every clustered face — used to learn where each old
/// cluster's faces landed after a re-cluster, so names can follow them.
pub fn face_clusters(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    let mut stmt =
        conn.prepare("SELECT id, cluster_id FROM faces WHERE cluster_id IS NOT NULL")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Apply a full re-cluster: set every face's cluster id in one transaction. Pairs
/// are `(face_id, cluster_id)`.
pub fn set_face_clusters(conn: &mut Connection, pairs: &[(i64, i64)]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut up = tx.prepare("UPDATE faces SET cluster_id = ?1 WHERE id = ?2")?;
        for (face_id, cluster_id) in pairs {
            up.execute(rusqlite::params![cluster_id, face_id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Replace the whole `cluster_names` table with `(cluster_id, name)` pairs — used
/// after a re-cluster to re-anchor each surviving name to its new cluster id. Done
/// in one transaction so People never sees a half-named state.
pub fn replace_cluster_names(conn: &mut Connection, names: &[(i64, String)]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        tx.execute("DELETE FROM cluster_names", [])?;
        let mut ins = tx.prepare(
            "INSERT INTO cluster_names (cluster_id, name) VALUES (?1, ?2)
             ON CONFLICT(cluster_id) DO UPDATE SET name = excluded.name",
        )?;
        for (cluster_id, name) in names {
            ins.execute(rusqlite::params![cluster_id, name])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// The highest-confidence face ids for a cluster (detector `score` desc) — the
/// example faces shown on a merge card so one glance decides.
pub fn top_face_ids(conn: &Connection, cluster_id: i64, limit: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM faces WHERE cluster_id = ?1 ORDER BY score DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![cluster_id, limit], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Discard all detected faces and re-arm the sweep. Used as a one-time migration
/// when the embedding pipeline itself changes (e.g. the alignment fix): every
/// stored embedding is now invalid, and landmarks aren't persisted, so faces must
/// be re-detected from scratch. Clears names and the recluster flag too, since
/// they're keyed to the old (bad) clusters.
pub fn reset_faces_for_recompute(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM faces;
         DELETE FROM cluster_names;
         DELETE FROM identities;
         DELETE FROM cannot_link;
         UPDATE photos SET faces_scanned = 0;
         DELETE FROM app_meta WHERE key = 'reclustered_v1';",
    )?;
    Ok(())
}

/// The highest-confidence embeddings of an identity — a compact "anchor profile"
/// the magnet matches other clusters against. Capped via `limit` because a few
/// dozen good exemplars characterize a person as well as hundreds.
pub fn identity_anchor_embeddings(
    conn: &Connection,
    identity_id: i64,
    limit: i64,
) -> Result<Vec<Vec<f32>>> {
    let mut stmt = conn.prepare(
        "SELECT embedding FROM faces WHERE identity_id = ?1 ORDER BY score DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![identity_id, limit], |r| {
        let blob: Vec<u8> = r.get(0)?;
        Ok(decode_embedding(&blob))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The clusters an identity's faces currently sit in, largest first. The first is
/// the natural "into" target when absorbing look-alike groups.
pub fn clusters_of_identity(conn: &Connection, identity_id: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT cluster_id, COUNT(*) c FROM faces
         WHERE identity_id = ?1 AND cluster_id IS NOT NULL
         GROUP BY cluster_id ORDER BY c DESC",
    )?;
    let rows = stmt.query_map([identity_id], |r| r.get::<_, i64>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Read an app-level key/value flag (e.g. "have we run the one-time re-cluster").
pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM app_meta WHERE key = ?1", [key], |r| r.get(0))
        .ok())
}

/// Set an app-level key/value flag.
pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO app_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
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

/// Every named cluster as `(cluster_id, name)` — used to carry names across a
/// re-cluster.
pub fn cluster_names_all(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare("SELECT cluster_id, name FROM cluster_names")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Name (or rename) a cluster. Empty name clears it. Naming also confirms an
/// identity: it binds every face in the cluster to one durable `identity_id` (so
/// the name — and the grouping — survives the next re-cluster) and stores the name
/// on that identity.
pub fn name_cluster(conn: &Connection, cluster_id: i64, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        conn.execute("DELETE FROM cluster_names WHERE cluster_id = ?1", [cluster_id])?;
        if let Some(id) = identity_of_cluster(conn, cluster_id)? {
            conn.execute("UPDATE identities SET name = NULL WHERE id = ?1", [id])?;
        }
        return Ok(());
    }
    conn.execute(
        "INSERT INTO cluster_names (cluster_id, name) VALUES (?1, ?2)
         ON CONFLICT(cluster_id) DO UPDATE SET name = excluded.name",
        rusqlite::params![cluster_id, name],
    )?;
    let id = ensure_identity_for_cluster(conn, cluster_id)?;
    conn.execute("UPDATE identities SET name = ?1 WHERE id = ?2", rusqlite::params![name, id])?;
    Ok(())
}

/// Merge cluster `from` into `into`: reassign its faces and drop its name. The
/// surviving cluster keeps `into`'s name. The merge is also recorded durably: all
/// faces end up under `into`'s identity, a must-link that the next re-cluster
/// honors (so you never have to merge the same two people twice).
pub fn merge_clusters(conn: &Connection, into: i64, from: i64) -> Result<()> {
    conn.execute(
        "UPDATE faces SET cluster_id = ?1 WHERE cluster_id = ?2",
        rusqlite::params![into, from],
    )?;
    conn.execute("DELETE FROM cluster_names WHERE cluster_id = ?1", [from])?;
    // Fold any pre-existing identity on the `from` side into `into`'s identity, then
    // bind every face of the (now combined) cluster to it.
    let into_id = ensure_identity_for_cluster(conn, into)?;
    conn.execute(
        "UPDATE faces SET identity_id = ?1 WHERE cluster_id = ?2",
        rusqlite::params![into_id, into],
    )?;
    Ok(())
}

/// The identity bound to a cluster, if any (the one most of its faces carry).
pub fn identity_of_cluster(conn: &Connection, cluster_id: i64) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT identity_id FROM faces
             WHERE cluster_id = ?1 AND identity_id IS NOT NULL
             GROUP BY identity_id ORDER BY COUNT(*) DESC LIMIT 1",
            [cluster_id],
            |r| r.get(0),
        )
        .ok())
}

/// Get (or create) the identity for a cluster and bind every face in the cluster
/// to it. Reuses an existing identity already present on the cluster so repeated
/// naming/merging doesn't spawn duplicates.
pub fn ensure_identity_for_cluster(conn: &Connection, cluster_id: i64) -> Result<i64> {
    let id = match identity_of_cluster(conn, cluster_id)? {
        Some(id) => id,
        None => {
            conn.execute("INSERT INTO identities (name) VALUES (NULL)", [])?;
            conn.last_insert_rowid()
        }
    };
    conn.execute(
        "UPDATE faces SET identity_id = ?1 WHERE cluster_id = ?2",
        rusqlite::params![id, cluster_id],
    )?;
    Ok(id)
}

/// (face_id, identity_id) for every face the user has bound to an identity — the
/// must-link constraints a re-cluster must honor.
pub fn face_identities(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    let mut stmt =
        conn.prepare("SELECT id, identity_id FROM faces WHERE identity_id IS NOT NULL")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// (identity_id, name) for every identity that has been given a name.
pub fn named_identities(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare("SELECT id, name FROM identities WHERE name IS NOT NULL")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Record that two clusters are *not* the same person (from a declined merge),
/// promoting each side to a durable identity so the barrier survives re-clusters.
/// Stored as an unordered identity pair.
pub fn add_cannot_link(conn: &Connection, into: i64, from: i64) -> Result<()> {
    let a = ensure_identity_for_cluster(conn, into)?;
    let b = ensure_identity_for_cluster(conn, from)?;
    add_cannot_link_ids(conn, a, b)
}

/// Record a cannot-link directly between two **identity** ids (a < b normalized).
/// Used by reassign, where both identities already exist.
pub fn add_cannot_link_ids(conn: &Connection, a: i64, b: i64) -> Result<()> {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    conn.execute(
        "INSERT OR IGNORE INTO cannot_link (a, b) VALUES (?1, ?2)",
        rusqlite::params![lo, hi],
    )?;
    Ok(())
}

/// All declared "not the same" identity pairs (unordered, a < b).
pub fn cannot_link_pairs(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare("SELECT a, b FROM cannot_link")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Every photo containing this person, newest first — the same ordering as the
/// timeline, so the person page reads as a filtered timeline. One row per photo
/// (a photo with two of their faces still appears once).
pub fn person_photos(conn: &Connection, cluster_id: i64) -> Result<Vec<PhotoRow>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.thumb_status, COALESCE(p.taken_ts, p.mtime) AS ts
         FROM photos p
         JOIN faces f ON f.photo_id = p.id
         WHERE f.cluster_id = ?1
         GROUP BY p.id
         ORDER BY ts DESC, p.id DESC",
    )?;
    let rows = stmt.query_map([cluster_id], |r| {
        Ok(PhotoRow {
            id: r.get(0)?,
            status: r.get(1)?,
            ts: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------------------
// Face corrections — the shared primitive behind reassign / ignore, acting on a
// set of face ids. The person page and the in-photo overlay differ only in how
// they pick that set. Every correction touches `identity_id` (the durable
// must-link), never just `cluster_id`, so it survives the next re-cluster.
// ---------------------------------------------------------------------------

/// A face within one photo, for the in-photo overlay: its id, bounding box, and
/// the person it currently belongs to (cluster id + name, if named). Ignored
/// faces are omitted — we excluded them on purpose, so we don't redraw them.
#[derive(serde::Serialize)]
pub struct PhotoFace {
    pub face_id: i64,
    pub cluster_id: Option<i64>,
    pub name: Option<String>,
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

pub fn faces_in_photo(conn: &Connection, photo_id: i64) -> Result<Vec<PhotoFace>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.cluster_id,
                (SELECT name FROM cluster_names cn WHERE cn.cluster_id = f.cluster_id),
                f.x1, f.y1, f.x2, f.y2
         FROM faces f
         WHERE f.photo_id = ?1 AND f.ignored = 0
         ORDER BY f.score DESC",
    )?;
    let rows = stmt.query_map([photo_id], |r| {
        Ok(PhotoFace {
            face_id: r.get(0)?,
            cluster_id: r.get(1)?,
            name: r.get(2)?,
            x1: r.get(3)?,
            y1: r.get(4)?,
            x2: r.get(5)?,
            y2: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// A face's grouping before a correction, captured so the action can be undone
/// exactly — including whether it was ignored.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct FaceState {
    pub face_id: i64,
    pub cluster_id: Option<i64>,
    pub identity_id: Option<i64>,
    pub ignored: bool,
}

/// Build a SQL placeholder list (`?,?,…`) for an `IN (…)` clause.
fn placeholders(n: usize) -> String {
    std::iter::repeat("?").take(n).collect::<Vec<_>>().join(",")
}

/// Snapshot the current grouping of each face id, for undo.
pub fn capture_face_states(conn: &Connection, face_ids: &[i64]) -> Result<Vec<FaceState>> {
    if face_ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT id, cluster_id, identity_id, ignored FROM faces WHERE id IN ({})",
        placeholders(face_ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(face_ids.iter()), |r| {
        Ok(FaceState {
            face_id: r.get(0)?,
            cluster_id: r.get(1)?,
            identity_id: r.get(2)?,
            ignored: r.get::<_, i64>(3)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Undo a correction: put each face back exactly where it was.
pub fn restore_face_states(conn: &mut Connection, states: &[FaceState]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut up = tx.prepare(
            "UPDATE faces SET cluster_id = ?1, identity_id = ?2, ignored = ?3 WHERE id = ?4",
        )?;
        for s in states {
            up.execute(rusqlite::params![s.cluster_id, s.identity_id, s.ignored as i64, s.face_id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// The face ids belonging to `cluster_id` within any of `photo_ids` — resolves a
/// person-page multi-selection (one cell per photo) to the actual faces to act on.
pub fn face_ids_in_photos_for_cluster(
    conn: &Connection,
    photo_ids: &[i64],
    cluster_id: i64,
) -> Result<Vec<i64>> {
    if photo_ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT id FROM faces WHERE cluster_id = ?1 AND photo_id IN ({})",
        placeholders(photo_ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(photo_ids.len() + 1);
    params.push(&cluster_id);
    for id in photo_ids {
        params.push(id);
    }
    let rows = stmt.query_map(params.as_slice(), |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Next free transient cluster id — for splitting faces off into a brand-new
/// person before the next re-cluster re-numbers everything anyway.
pub fn next_cluster_id(conn: &Connection) -> Result<i64> {
    // MAX over an empty/all-NULL column yields a single NULL row, read as None.
    let max: Option<i64> = conn.query_row("SELECT MAX(cluster_id) FROM faces", [], |r| r.get(0))?;
    Ok(max.unwrap_or(0) + 1)
}

/// Move a set of faces onto a person: set their cluster and durable identity in
/// one transaction (and clear any `ignored` flag). The identity binding is the
/// must-link that makes the move survive re-clustering.
pub fn set_faces_person(
    conn: &mut Connection,
    face_ids: &[i64],
    cluster_id: i64,
    identity_id: i64,
) -> Result<()> {
    if face_ids.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut up = tx.prepare(
            "UPDATE faces SET cluster_id = ?1, identity_id = ?2, ignored = 0 WHERE id = ?3",
        )?;
        for id in face_ids {
            up.execute(rusqlite::params![cluster_id, identity_id, id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Ignore a set of faces: drop them from People for good. Cluster and identity go
/// NULL and `ignored` is set, so they leave every grouping and the overlay.
pub fn ignore_faces(conn: &Connection, face_ids: &[i64]) -> Result<()> {
    if face_ids.is_empty() {
        return Ok(());
    }
    let sql = format!(
        "UPDATE faces SET cluster_id = NULL, identity_id = NULL, ignored = 1 WHERE id IN ({})",
        placeholders(face_ids.len())
    );
    conn.execute(&sql, rusqlite::params_from_iter(face_ids.iter()))?;
    Ok(())
}

/// Whether a cannot-link already exists for this (unordered) identity pair.
pub fn cannot_link_exists(conn: &Connection, a: i64, b: i64) -> Result<bool> {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM cannot_link WHERE a = ?1 AND b = ?2",
        rusqlite::params![lo, hi],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Drop a cannot-link (undo of a reassign that recorded one).
pub fn remove_cannot_link(conn: &Connection, a: i64, b: i64) -> Result<()> {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    conn.execute(
        "DELETE FROM cannot_link WHERE a = ?1 AND b = ?2",
        rusqlite::params![lo, hi],
    )?;
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

/// A page of STATUS_CLOUD photos with id > `after_id`, ordered by id. Used by
/// the proactive cloud backfill to walk the library in order without re-queuing
/// items already processed in this pass.
pub fn cloud_jobs_after(conn: &Connection, after_id: i64, limit: i64) -> Result<Vec<Job>> {
    let mut stmt = conn.prepare(
        "SELECT id, path FROM photos WHERE thumb_status = ?1 AND id > ?2 ORDER BY id LIMIT ?3",
    )?;
    let rows = stmt.query_map(rusqlite::params![STATUS_CLOUD, after_id, limit], |r| {
        Ok(Job { id: r.get(0)?, path: r.get(1)? })
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
