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
    // User-confirmed exceptions to the same-photo rule: face-id pairs that share a
    // photo but ARE one person — a collage, a mirror, a photo-booth strip. Granted
    // from the review question ("same photo — same person?"), per pair, so one
    // collage never weakens the twins-stay-apart rule anywhere else. Photo-level
    // truth (like embeddings), so "start people over" deliberately keeps these.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS same_photo_ok (
            a INTEGER NOT NULL,
            b INTEGER NOT NULL,
            PRIMARY KEY (a, b)
         );",
    )?;
    // `ignored` faces are excluded from People for good (a stranger, a poster, a
    // reflection) — they keep cluster_id = NULL like a detach, but the flag marks
    // the exclusion as intentional and permanent so the overlay never re-draws them.
    let _ = conn.execute("ALTER TABLE faces ADD COLUMN ignored INTEGER NOT NULL DEFAULT 0", []);
    // `confirmed` marks faces the *user* vouched for (named / moved / merged), as
    // opposed to ones the machine auto-folded in. Only confirmed faces are must-links
    // (so auto-folds stay free to re-home), and only they are anchor exemplars (so the
    // magnet learns from your labels). This is what lets naming a second person eject
    // the first person's wrongly-folded look-alikes.
    let _ = conn.execute("ALTER TABLE faces ADD COLUMN confirmed INTEGER NOT NULL DEFAULT 0", []);
    // One-time backfill: treat every existing identity-bound face as confirmed, so a
    // library curated before this column existed isn't blown away on the next
    // re-cluster (which now honors only confirmed must-links). Guarded so it runs once.
    if get_meta(conn, "confirmed_backfill_v1")?.is_none() {
        conn.execute("UPDATE faces SET confirmed = 1 WHERE identity_id IS NOT NULL", [])?;
        set_meta(conn, "confirmed_backfill_v1", "1")?;
    }
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

/// (cluster_id, photo_id, embedding) for every already-clustered face — used to
/// rebuild the in-memory cluster index at startup (photo id feeds the same-photo
/// exclusion during incremental assignment).
pub fn clustered_embeddings(conn: &Connection) -> Result<Vec<(i64, i64, Vec<f32>)>> {
    let mut stmt = conn
        .prepare("SELECT cluster_id, photo_id, embedding FROM faces WHERE cluster_id IS NOT NULL")?;
    let rows = stmt.query_map([], |r| {
        let cid: i64 = r.get(0)?;
        let pid: i64 = r.get(1)?;
        let blob: Vec<u8> = r.get(2)?;
        Ok((cid, pid, decode_embedding(&blob)))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// (photo_id, face_id, x1, y1, x2, y2) for every clustered face in a photo that
/// holds 2+ clustered faces — the raw material for the same-photo exclusion
/// (two faces in one photo are two different people, unless their boxes overlap
/// so much they're a double detection of one face).
pub fn multi_face_boxes(conn: &Connection) -> Result<Vec<(i64, i64, f32, f32, f32, f32)>> {
    let mut stmt = conn.prepare(
        "SELECT photo_id, id, x1, y1, x2, y2 FROM faces
         WHERE cluster_id IS NOT NULL
           AND photo_id IN (
             SELECT photo_id FROM faces WHERE cluster_id IS NOT NULL
             GROUP BY photo_id HAVING COUNT(*) > 1)
         ORDER BY photo_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// (cluster_id, photo_id) for every clustered face — for co-occurrence vetoes on
/// suggestions and auto-fold (a candidate group photographed alongside the person
/// cannot BE the person).
pub fn cluster_photo_pairs(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    let mut stmt = conn
        .prepare("SELECT cluster_id, photo_id FROM faces WHERE cluster_id IS NOT NULL")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// (identity_id, photo_id) for every *confirmed* face — the person side of the
/// co-occurrence veto.
pub fn confirmed_identity_photos(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT identity_id, photo_id FROM faces
         WHERE confirmed = 1 AND identity_id IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The user-confirmed same-photo exceptions (collages/mirrors), as normalized pairs.
pub fn same_photo_ok_pairs(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare("SELECT a, b FROM same_photo_ok")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Record same-photo exceptions for a set of face pairs (normalized, idempotent).
pub fn add_same_photo_ok(conn: &Connection, pairs: &[(i64, i64)]) -> Result<()> {
    let mut ins = conn
        .prepare("INSERT OR IGNORE INTO same_photo_ok (a, b) VALUES (?1, ?2)")?;
    for &(x, y) in pairs {
        let (a, b) = if x < y { (x, y) } else { (y, x) };
        ins.execute(rusqlite::params![a, b])?;
    }
    Ok(())
}

/// Face pairs across two clusters that share a photo: `(photo_id, face_in_a,
/// face_in_b)`. These are the pairs the same-photo rule blocks — and the ones a
/// "same person (collage)" answer marks as exceptions.
pub fn cooccurring_face_pairs(
    conn: &Connection,
    cluster_a: i64,
    cluster_b: i64,
) -> Result<Vec<(i64, i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT f1.photo_id, f1.id, f2.id FROM faces f1
         JOIN faces f2 ON f2.photo_id = f1.photo_id
         WHERE f1.cluster_id = ?1 AND f2.cluster_id = ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![cluster_a, cluster_b], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
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

/// Every face id in a cluster, best (highest score) first. Backs the "Who is this?"
/// split grid, where the user tags each contested face as one candidate or the other,
/// so — unlike [`top_face_ids`] — it must return the whole cluster, not a sample.
pub fn cluster_face_ids(conn: &Connection, cluster_id: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM faces WHERE cluster_id = ?1 ORDER BY score DESC",
    )?;
    let rows = stmt.query_map(rusqlite::params![cluster_id], |r| r.get(0))?;
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
         DELETE FROM same_photo_ok;
         UPDATE photos SET faces_scanned = 0;
         DELETE FROM app_meta WHERE key = 'reclustered_v1';",
    )?;
    Ok(())
}

/// Clear every *decision* — identities, names, cannot-links — while keeping the
/// detected faces and their embeddings, then reset each face to its own singleton
/// cluster (also un-ignoring any). A fresh unsupervised re-cluster then starts from a
/// truly clean slate, with no re-detection needed — the fast "start people over".
pub fn clear_face_decisions(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM identities;
         DELETE FROM cluster_names;
         DELETE FROM cannot_link;
         UPDATE faces SET identity_id = NULL, ignored = 0, confirmed = 0, cluster_id = id;",
    )?;
    Ok(())
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

/// The cluster of an existing person by name (case-insensitive), if one exists — so
/// "move to a new person: Mía" merges into the real Mía instead of minting a duplicate.
pub fn cluster_for_name(conn: &Connection, name: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT cluster_id FROM cluster_names WHERE lower(name) = lower(?1) LIMIT 1",
            [name.trim()],
            |r| r.get(0),
        )
        .ok())
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
    // Naming a cluster vouches for its current contents — they become confirmed
    // exemplars (must-links + anchor), the label the magnet learns from. Scoped to
    // faces bound to THIS identity: confirming a stray face another person's
    // identity still owns would mint bogus exemplars for *them*.
    conn.execute(
        "UPDATE faces SET confirmed = 1 WHERE cluster_id = ?1 AND identity_id = ?2",
        rusqlite::params![cluster_id, id],
    )?;
    Ok(())
}

/// True if the cluster holds faces the user confirmed under an identity other
/// than `identity` — i.e. absorbing or offering it would swallow (part of) a
/// different person. `identity = None` means any confirmed identity is foreign.
pub fn cluster_has_foreign_confirmed(
    conn: &Connection,
    cluster_id: i64,
    identity: Option<i64>,
) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM faces
         WHERE cluster_id = ?1 AND confirmed = 1
           AND identity_id IS NOT NULL AND identity_id IS NOT ?2",
        rusqlite::params![cluster_id, identity],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Like [`cluster_has_foreign_confirmed`], but only *named* identities count — a
/// real person the user labeled. Faces confirmed under an unnamed competitor
/// (bookkeeping minted by a rejection) don't block an explicit assignment: the
/// user's direct judgment on the faces outranks that bookkeeping.
pub fn cluster_has_named_foreign_confirmed(
    conn: &Connection,
    cluster_id: i64,
    identity: Option<i64>,
) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM faces f JOIN identities i ON i.id = f.identity_id
         WHERE f.cluster_id = ?1 AND f.confirmed = 1
           AND i.name IS NOT NULL AND f.identity_id IS NOT ?2",
        rusqlite::params![cluster_id, identity],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Rebind a cluster's faces confirmed under *unnamed* identities to `new_identity`.
/// Used when the user explicitly assigns the cluster to a person: the unnamed
/// competitor was minted by an earlier rejection, and without this adoption the
/// assignment would be refused forever (the stuck-card loop).
pub fn adopt_unnamed_confirmed(
    conn: &Connection,
    cluster_id: i64,
    new_identity: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE faces SET identity_id = ?2
         WHERE cluster_id = ?1 AND confirmed = 1 AND identity_id != ?2
           AND identity_id IN (SELECT id FROM identities WHERE name IS NULL)",
        rusqlite::params![cluster_id, new_identity],
    )?;
    Ok(())
}

/// Mark a cluster's faces as user-confirmed (exemplars + must-links) under the
/// identity they're being vouched as: unclaimed faces and that identity's own.
/// A face bound to a *different* identity is untouched — confirming it here would
/// mint bogus exemplars for the other person. Used before absorbs/merges/rejects
/// so the vouched-for faces become sticky, not auto-ejectable.
pub fn confirm_cluster_faces(
    conn: &Connection,
    cluster_id: i64,
    identity: Option<i64>,
) -> Result<()> {
    conn.execute(
        "UPDATE faces SET confirmed = 1
         WHERE cluster_id = ?1 AND (identity_id IS NULL OR identity_id IS ?2)",
        rusqlite::params![cluster_id, identity],
    )?;
    Ok(())
}

/// (face_id, identity_id) for every *confirmed* face — the must-link constraints a
/// re-cluster honors. Auto-folded faces are excluded so they stay free to re-home.
pub fn confirmed_face_identities(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    let mut stmt = conn
        .prepare("SELECT id, identity_id FROM faces WHERE identity_id IS NOT NULL AND confirmed = 1")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The highest-confidence *confirmed* exemplars of an identity — the anchor profile the
/// magnet matches against. Confirmed-only so the anchor is what the user taught, never
/// drifting from the machine's own guesses.
pub fn confirmed_anchor_embeddings(conn: &Connection, identity_id: i64, limit: i64) -> Result<Vec<Vec<f32>>> {
    let mut stmt = conn.prepare(
        "SELECT embedding FROM faces WHERE identity_id = ?1 AND confirmed = 1 ORDER BY score DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![identity_id, limit], |r| {
        let blob: Vec<u8> = r.get(0)?;
        Ok(decode_embedding(&blob))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Clear the identity binding on every *unconfirmed* (auto-folded) face — wiping the
/// machine's tentative labels before they're re-derived competitively. User labels
/// (confirmed) are untouched.
pub fn clear_unconfirmed_identities(conn: &Connection) -> Result<()> {
    conn.execute("UPDATE faces SET identity_id = NULL WHERE confirmed = 0", [])?;
    Ok(())
}

/// Every identity with at least one confirmed face — each user-labeled person, named
/// or not, that can compete for faces. A "not Mía" split mints an unnamed one of these
/// so look-alikes get pulled toward it and away from Mía.
pub fn confirmed_identity_ids(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT identity_id FROM faces WHERE confirmed = 1 AND identity_id IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Identities with at least `min_count` confirmed faces — enough evidence to *claim*
/// (absorb) look-alikes, not just compete defensively for them.
pub fn fold_eligible_identities(conn: &Connection, min_count: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT identity_id FROM faces WHERE confirmed = 1 AND identity_id IS NOT NULL
         GROUP BY identity_id HAVING COUNT(*) >= ?1",
    )?;
    let rows = stmt.query_map([min_count], |r| r.get::<_, i64>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Clusters that hold a confirmed face — the "owned" people-clusters auto-fold must
/// never fold *into* another (it would merge two confirmed people).
pub fn confirmed_clusters(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT cluster_id FROM faces WHERE confirmed = 1 AND cluster_id IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Merge cluster `from` into `into`: reassign its faces and drop its name. The
/// surviving cluster keeps `into`'s name and identity. The merge is recorded
/// durably: the absorbed faces end up under `into`'s identity, a must-link the
/// next re-cluster honors (so you never have to merge the same two people twice).
///
/// Identities are resolved BEFORE any face moves. The old version derived the
/// winner from the *combined* cluster's plurality and rebound every face to it —
/// so merging a big cluster into a small one let the big side hijack the result,
/// rebinding even faces the user had confirmed under a different identity (the
/// Mía/Camila incident: Mía's own confirmed faces became "Camila", and Mía
/// vanished). Now a face confirmed under a different identity is never rebound.
pub fn merge_clusters(conn: &Connection, into: i64, from: i64) -> Result<()> {
    let from_identity = identity_of_cluster(conn, from)?;
    let into_identity = ensure_identity_for_cluster(conn, into)?;
    conn.execute(
        "UPDATE faces SET cluster_id = ?1 WHERE cluster_id = ?2",
        rusqlite::params![into, from],
    )?;
    conn.execute("DELETE FROM cluster_names WHERE cluster_id = ?1", [from])?;
    // Bind the combined cluster to `into`'s identity: unclaimed faces, faces that
    // carried `from`'s identity (the user just vouched they're the same person),
    // and tentative machine labels. Confirmed faces of any *other* identity stay.
    conn.execute(
        "UPDATE faces SET identity_id = ?1
         WHERE cluster_id = ?2
           AND (identity_id IS NULL OR identity_id = ?3 OR confirmed = 0)",
        rusqlite::params![into_identity, into, from_identity],
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

/// Get (or create) the identity for a cluster and bind the cluster's faces to it.
/// Reuses an existing identity already present on the cluster so repeated
/// naming/merging doesn't spawn duplicates. Binding never touches a face the user
/// confirmed under a *different* identity — a cluster can (transiently) hold two
/// people's confirmed faces, and stamping the plurality identity over the
/// minority's steals them (see `merge_clusters`).
pub fn ensure_identity_for_cluster(conn: &Connection, cluster_id: i64) -> Result<i64> {
    let id = match identity_of_cluster(conn, cluster_id)? {
        Some(id) => id,
        None => {
            conn.execute("INSERT INTO identities (name) VALUES (NULL)", [])?;
            conn.last_insert_rowid()
        }
    };
    conn.execute(
        "UPDATE faces SET identity_id = ?1
         WHERE cluster_id = ?2 AND (identity_id IS NULL OR confirmed = 0)",
        rusqlite::params![id, cluster_id],
    )?;
    Ok(id)
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

/// (face_id, photo_id, ts, detector_score, embedding) for every face in a person's
/// cluster — the input to the person-page "looks" (intra-identity sub-clustering).
/// Ordered oldest-first so the look grouping is stable and reads chronologically.
pub fn person_faces(conn: &Connection, cluster_id: i64) -> Result<Vec<(i64, i64, i64, f32, Vec<f32>)>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.photo_id, COALESCE(p.taken_ts, p.mtime) AS ts, f.score, f.embedding
         FROM faces f
         JOIN photos p ON p.id = f.photo_id
         WHERE f.cluster_id = ?1
         ORDER BY ts ASC, f.id ASC",
    )?;
    let rows = stmt.query_map([cluster_id], |r| {
        let blob: Vec<u8> = r.get(4)?;
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, decode_embedding(&blob)))
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
/// exactly — including whether it was ignored and whether the *user* had vouched
/// for it. `confirmed` must round-trip: corrections set it, so an undo that left
/// it behind would promote a machine guess to a user-confirmed must-link (and
/// anchor exemplar) under the restored identity — vouching forged by an action
/// the user took back.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct FaceState {
    pub face_id: i64,
    pub cluster_id: Option<i64>,
    pub identity_id: Option<i64>,
    pub ignored: bool,
    #[serde(default)]
    pub confirmed: bool,
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
        "SELECT id, cluster_id, identity_id, ignored, confirmed FROM faces WHERE id IN ({})",
        placeholders(face_ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(face_ids.iter()), |r| {
        Ok(FaceState {
            face_id: r.get(0)?,
            cluster_id: r.get(1)?,
            identity_id: r.get(2)?,
            ignored: r.get::<_, i64>(3)? != 0,
            confirmed: r.get::<_, i64>(4)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Undo a correction: put each face back exactly where it was.
pub fn restore_face_states(conn: &mut Connection, states: &[FaceState]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut up = tx.prepare(
            "UPDATE faces SET cluster_id = ?1, identity_id = ?2, ignored = ?3, confirmed = ?4 WHERE id = ?5",
        )?;
        for s in states {
            up.execute(rusqlite::params![
                s.cluster_id,
                s.identity_id,
                s.ignored as i64,
                s.confirmed as i64,
                s.face_id
            ])?;
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
        // A deliberate move is a user label: confirm it (sticky exemplar, must-link).
        let mut up = tx.prepare(
            "UPDATE faces SET cluster_id = ?1, identity_id = ?2, ignored = 0, confirmed = 1 WHERE id = ?3",
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

/// Detach faces from their person without saying who they are: clear the identity
/// must-link and scatter each into its own fresh cluster (starting at `base_cluster`),
/// so the next re-cluster re-homes each by appearance. Unlike `ignore_faces` they stay
/// in the pool (cluster_id non-NULL), and unlike a new-person split they're not forced
/// together. Backs "not this person / not <name>".
pub fn detach_faces(conn: &mut Connection, face_ids: &[i64], base_cluster: i64) -> Result<()> {
    if face_ids.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut up = tx.prepare(
            "UPDATE faces SET cluster_id = ?1, identity_id = NULL, ignored = 0, confirmed = 0 WHERE id = ?2",
        )?;
        for (i, id) in face_ids.iter().enumerate() {
            up.execute(rusqlite::params![base_cluster + i as i64, id])?;
        }
    }
    tx.commit()?;
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
    let sql = format!("DELETE FROM faces WHERE photo_id IN ({})", placeholders(photo_ids.len()));
    conn.execute(&sql, rusqlite::params_from_iter(photo_ids.iter()))?;
    Ok(())
}

/// Photo ids whose original is a genuine HEIC/HEIF (by extension). Drives the
/// one-time orientation-repair migration.
pub fn heic_photo_ids(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM photos WHERE lower(path) LIKE '%.heic' OR lower(path) LIKE '%.heif'",
    )?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Face ids belonging to the given photos — used to clean their cached crops (keyed
/// by face id) before the rows are deleted.
pub fn face_ids_of_photos(conn: &Connection, photo_ids: &[i64]) -> Result<Vec<i64>> {
    if photo_ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!("SELECT id FROM faces WHERE photo_id IN ({})", placeholders(photo_ids.len()));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(photo_ids.iter()), |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Re-arm the given photos for a fresh face sweep and thumbnail regeneration: drop
/// their detected faces, clear `faces_scanned`, and knock any cached (stale-
/// orientation) thumbnail back to PENDING so the worker regenerates it upright.
/// Callers drop the stale preview/crop files on the filesystem side.
pub fn rearm_photos_for_redetect(conn: &Connection, photo_ids: &[i64]) -> Result<()> {
    if photo_ids.is_empty() {
        return Ok(());
    }
    delete_faces_for_photos(conn, photo_ids)?;
    let ph = placeholders(photo_ids.len());
    conn.execute(
        &format!("UPDATE photos SET faces_scanned = 0 WHERE id IN ({ph})"),
        rusqlite::params_from_iter(photo_ids.iter()),
    )?;
    // Only re-queue photos that already have a local thumbnail; leave cloud-only
    // ones (never downloaded) untouched so this doesn't trigger a mass download.
    conn.execute(
        &format!(
            "UPDATE photos SET thumb_status = {STATUS_PENDING} \
             WHERE thumb_status = {STATUS_READY} AND id IN ({ph})"
        ),
        rusqlite::params_from_iter(photo_ids.iter()),
    )?;
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
    let sql = format!(
        "SELECT id, thumb_status, path FROM photos WHERE id IN ({})",
        placeholders(ids.len())
    );
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
    let sql = format!(
        "UPDATE photos SET thumb_status = {status} WHERE id IN ({})",
        placeholders(ids.len())
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        conn
    }

    fn insert_face(conn: &Connection, id: i64, cluster: i64, identity: Option<i64>, confirmed: bool) {
        conn.execute(
            "INSERT INTO faces (id, photo_id, x1, y1, x2, y2, score, embedding, cluster_id, identity_id, confirmed)
             VALUES (?1, 1, 0, 0, 1, 1, 0.9, x'00000000', ?2, ?3, ?4)",
            rusqlite::params![id, cluster, identity, confirmed as i64],
        )
        .unwrap();
    }

    fn identity_of_face(conn: &Connection, face: i64) -> Option<i64> {
        conn.query_row("SELECT identity_id FROM faces WHERE id = ?1", [face], |r| r.get(0))
            .unwrap()
    }

    /// The Mía/Camila incident: merging a BIG cluster into a small one must not let
    /// the big side's plurality hijack the small side's confirmed identity. The old
    /// code re-derived the combined cluster's identity after the move and rebound
    /// every face to it — Mía's own confirmed faces became "Camila" and Mía
    /// vanished from the grid.
    #[test]
    fn merge_transfers_from_side_but_never_steals_other_confirmed() {
        let conn = test_conn();
        conn.execute_batch(
            "INSERT INTO identities (id, name) VALUES (1, 'Mía'), (2, 'Camila'), (3, 'Lianny');",
        )
        .unwrap();
        // Mía: 2 confirmed faces in cluster 10. A stray confirmed Lianny face also
        // sits in cluster 10 (transient states like this exist mid-correction).
        insert_face(&conn, 1, 10, Some(1), true);
        insert_face(&conn, 2, 10, Some(1), true);
        insert_face(&conn, 8, 10, Some(3), true);
        // Camila: 5 confirmed faces in cluster 20 (the bigger side), plus one
        // tentative machine-labeled face.
        for f in 3..=7 {
            insert_face(&conn, f, 20, Some(2), true);
        }
        insert_face(&conn, 9, 20, Some(2), false);

        // Explicit user merge: fold Camila's cluster INTO Mía's.
        merge_clusters(&conn, 10, 20).unwrap();

        // Mía's confirmed faces keep HER identity (the old code flipped them to 2).
        assert_eq!(identity_of_face(&conn, 1), Some(1));
        assert_eq!(identity_of_face(&conn, 2), Some(1));
        // The from-side faces transfer to Mía — that's what the merge means.
        for f in [3, 4, 5, 6, 7, 9] {
            assert_eq!(identity_of_face(&conn, f), Some(1), "face {f} should now be Mía");
        }
        // The stray confirmed Lianny face is untouched — never stolen by a merge.
        assert_eq!(identity_of_face(&conn, 8), Some(3));
    }

    /// ensure_identity_for_cluster must bind unclaimed/tentative faces without
    /// re-stamping faces the user confirmed under a different identity.
    #[test]
    fn ensure_identity_leaves_foreign_confirmed_faces_alone() {
        let conn = test_conn();
        conn.execute_batch("INSERT INTO identities (id, name) VALUES (1, 'Omar'), (2, 'Kevin');")
            .unwrap();
        insert_face(&conn, 1, 10, Some(1), true); // Omar, confirmed (plurality)
        insert_face(&conn, 2, 10, Some(1), true);
        insert_face(&conn, 6, 10, Some(1), true);
        insert_face(&conn, 3, 10, None, false); // unclaimed
        insert_face(&conn, 4, 10, Some(2), true); // Kevin, confirmed — must survive
        insert_face(&conn, 5, 10, Some(2), false); // tentative Kevin — rebindable

        let id = ensure_identity_for_cluster(&conn, 10).unwrap();
        assert_eq!(id, 1, "plurality identity is reused, no duplicate minted");
        assert_eq!(identity_of_face(&conn, 3), Some(1));
        assert_eq!(identity_of_face(&conn, 5), Some(1));
        assert_eq!(identity_of_face(&conn, 4), Some(2), "confirmed Kevin face stolen");
    }

    /// cluster_has_foreign_confirmed: the absorb-path guard.
    #[test]
    fn foreign_confirmed_detection() {
        let conn = test_conn();
        conn.execute_batch("INSERT INTO identities (id, name) VALUES (1, 'Omar'), (2, 'Kevin');")
            .unwrap();
        insert_face(&conn, 1, 10, Some(2), true);
        insert_face(&conn, 2, 20, Some(1), true);
        insert_face(&conn, 3, 30, None, false);
        // Cluster 10 holds Kevin-confirmed: foreign to Omar (1) and to "no identity".
        assert!(cluster_has_foreign_confirmed(&conn, 10, Some(1)).unwrap());
        assert!(cluster_has_foreign_confirmed(&conn, 10, None).unwrap());
        assert!(!cluster_has_foreign_confirmed(&conn, 10, Some(2)).unwrap());
        // Cluster 30 has no confirmed faces at all.
        assert!(!cluster_has_foreign_confirmed(&conn, 30, Some(1)).unwrap());
    }

    /// Undo must restore `confirmed` exactly. An auto-folded face (confirmed = 0)
    /// moved by the user (which confirms it) and then un-done must return to
    /// UNCONFIRMED under its old identity — leaving confirmed = 1 behind would
    /// mint a bogus user-vouched exemplar for the old identity out of an action
    /// the user reverted (and the reverse: undoing a detach must re-vouch).
    #[test]
    fn undo_restores_confirmed_flag() {
        let mut conn = test_conn();
        conn.execute_batch("INSERT INTO identities (id, name) VALUES (1, 'Omar'), (2, 'Kevin');")
            .unwrap();
        // Face 1: tentatively auto-folded to Omar. Face 2: user-confirmed Omar.
        insert_face(&conn, 1, 10, Some(1), false);
        insert_face(&conn, 2, 10, Some(1), true);
        let prior = capture_face_states(&conn, &[1, 2]).unwrap();

        // Move both to Kevin (a user label — sets confirmed = 1 on both), then
        // detach face 2 style-check: set_faces_person is the confirming path.
        set_faces_person(&mut conn, &[1, 2], 20, 2).unwrap();
        let confirmed_of = |conn: &Connection, id: i64| -> bool {
            conn.query_row("SELECT confirmed FROM faces WHERE id = ?1", [id], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
                != 0
        };
        assert!(confirmed_of(&conn, 1) && confirmed_of(&conn, 2));

        restore_face_states(&mut conn, &prior).unwrap();
        assert_eq!(identity_of_face(&conn, 1), Some(1));
        assert_eq!(identity_of_face(&conn, 2), Some(1));
        assert!(!confirmed_of(&conn, 1), "tentative face must return to unconfirmed");
        assert!(confirmed_of(&conn, 2), "user-vouched face must stay confirmed");
    }

    /// The same-photo exception round-trip: the pairs the rule blocks are exactly
    /// the ones a "same person (collage)" answer whitelists.
    #[test]
    fn same_photo_exception_roundtrip() {
        let conn = test_conn();
        // Photo 9 holds face 1 (cluster 10) and face 2 (cluster 20) — a collage
        // split. Photo 8 holds an unrelated pair in the same clusters.
        conn.execute("UPDATE faces SET photo_id = photo_id", []).unwrap(); // no-op guard
        conn.execute(
            "INSERT INTO faces (id, photo_id, x1, y1, x2, y2, score, embedding, cluster_id)
             VALUES (1, 9, 0,0,1,1, 0.9, x'00', 10), (2, 9, 2,2,3,3, 0.9, x'00', 20),
                    (3, 8, 0,0,1,1, 0.9, x'00', 10), (4, 7, 0,0,1,1, 0.9, x'00', 20)",
            [],
        )
        .unwrap();
        let pairs = cooccurring_face_pairs(&conn, 10, 20).unwrap();
        assert_eq!(pairs, vec![(9, 1, 2)], "only the shared-photo pair is blocked");
        add_same_photo_ok(&conn, &pairs.iter().map(|&(_, a, b)| (a, b)).collect::<Vec<_>>())
            .unwrap();
        add_same_photo_ok(&conn, &[(2, 1)]).unwrap(); // reversed + duplicate: idempotent
        assert_eq!(same_photo_ok_pairs(&conn).unwrap(), vec![(1, 2)]);
    }
}
