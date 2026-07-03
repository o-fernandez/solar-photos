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
            -- legacy (pre identity-centric grouping); names now live on
            -- identities. Kept so a downgrade doesn't lose the schema; unused.
            cluster_id INTEGER PRIMARY KEY,
            name       TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS app_meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
         );",
    )?;
    // Durable person records. Unlike cluster ids — which are reassigned from
    // scratch on every re-cluster — an identity id is permanent, so it carries
    // the user's decisions (this is so-and-so; these groups are the same person)
    // across re-clusters. A face's `identity_id` is both its display group (see
    // GROUP_EXPR below) and, when confirmed, the must-link a re-cluster honors.
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
    // The display-group key (see the "Display groups" section below): expression
    // index so per-person queries don't scan the whole faces table.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_faces_group ON faces(COALESCE(-identity_id, cluster_id));",
    )?;
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
    // reflection). They KEEP their appearance cluster_id — so an undo restores
    // them exactly — but every group query filters them out via `ignored = 0`.
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

// ---------------------------------------------------------------------------
// Display groups. A face shows under `COALESCE(-identity_id, cluster_id)`:
//   * **identity** (negative key, `-identity_id`) — the durable person. Stable
//     across every pass, because identity ids never change.
//   * **appearance cluster** (positive key, `cluster_id`) — the unsupervised
//     grouping. Renumbered only by a full re-cluster.
// Auto-fold and corrections write ONLY the identity layer; `cluster_id` belongs
// to the clustering passes alone (the batch re-cluster + the incremental index).
// That split is what makes self-heal a cheap re-derive — nothing ever has to be
// un-merged — and what makes a person's group id safe to hold in the UI: a
// positive key can only go stale across a (rare) re-cluster, and a negative key
// never does. Ignored faces keep their cluster_id (so undo is exact) but are
// excluded from every group query via `ignored = 0`.
// ---------------------------------------------------------------------------

/// The display-group key expression (documented above).
pub const GROUP_EXPR: &str = "COALESCE(-identity_id, cluster_id)";

/// (cluster_id, photo_id, embedding) for every in-pool face — used to rebuild
/// the in-memory cluster index at startup (photo id feeds the same-photo
/// exclusion during incremental assignment). Appearance layer, not groups.
pub fn clustered_embeddings(conn: &Connection) -> Result<Vec<(i64, i64, Vec<f32>)>> {
    let mut stmt = conn.prepare(
        "SELECT cluster_id, photo_id, embedding FROM faces WHERE cluster_id IS NOT NULL AND ignored = 0",
    )?;
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
         WHERE cluster_id IS NOT NULL AND ignored = 0
           AND photo_id IN (
             SELECT photo_id FROM faces WHERE cluster_id IS NOT NULL AND ignored = 0
             GROUP BY photo_id HAVING COUNT(*) > 1)
         ORDER BY photo_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// (group, photo_id) for every in-pool face — for co-occurrence vetoes on
/// suggestions and auto-fold (a candidate group photographed alongside the person
/// cannot BE the person).
pub fn cluster_photo_pairs(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {GROUP_EXPR}, photo_id FROM faces WHERE cluster_id IS NOT NULL AND ignored = 0"
    ))?;
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

/// Face pairs across two groups that share a photo: `(photo_id, face_in_a,
/// face_in_b)`. These are the pairs the same-photo rule blocks — and the ones a
/// "same person (collage)" answer marks as exceptions.
pub fn cooccurring_face_pairs(
    conn: &Connection,
    group_a: i64,
    group_b: i64,
) -> Result<Vec<(i64, i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT f1.photo_id, f1.id, f2.id FROM faces f1
         JOIN faces f2 ON f2.photo_id = f1.photo_id
         WHERE COALESCE(-f1.identity_id, f1.cluster_id) = ?1
           AND COALESCE(-f2.identity_id, f2.cluster_id) = ?2
           AND f1.ignored = 0 AND f2.ignored = 0",
    )?;
    let rows = stmt.query_map(rusqlite::params![group_a, group_b], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// (face_id, embedding) for every in-pool face — the input to a full
/// [`crate::cluster::recluster`]. Ignored faces stay out (a re-cluster must not
/// resurrect them); NULL-cluster rows are legacy out-of-pool state.
pub fn all_face_embeddings(conn: &Connection) -> Result<Vec<(i64, Vec<f32>)>> {
    let mut stmt = conn
        .prepare("SELECT id, embedding FROM faces WHERE cluster_id IS NOT NULL AND ignored = 0")?;
    let rows = stmt.query_map([], |r| {
        let id: i64 = r.get(0)?;
        let blob: Vec<u8> = r.get(1)?;
        Ok((id, decode_embedding(&blob)))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// (face_id, group, embedding) for every in-pool face — the input the
/// suggestion/fold engines reason over (display groups, not appearance).
pub fn face_cluster_embeddings(conn: &Connection) -> Result<Vec<(i64, i64, Vec<f32>)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, {GROUP_EXPR}, embedding FROM faces WHERE cluster_id IS NOT NULL AND ignored = 0"
    ))?;
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

/// The highest-confidence face ids for a group (detector `score` desc) — the
/// example faces shown on a merge card so one glance decides.
pub fn top_face_ids(conn: &Connection, group: i64, limit: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id FROM faces WHERE {GROUP_EXPR} = ?1 AND ignored = 0 ORDER BY score DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(rusqlite::params![group, limit], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Every face id in a group, best (highest score) first. Backs the "Who is this?"
/// split grid, where the user tags each contested face as one candidate or the other,
/// so — unlike [`top_face_ids`] — it must return the whole group, not a sample.
pub fn cluster_face_ids(conn: &Connection, group: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id FROM faces WHERE {GROUP_EXPR} = ?1 AND ignored = 0 ORDER BY score DESC"
    ))?;
    let rows = stmt.query_map(rusqlite::params![group], |r| r.get(0))?;
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

/// One person-group's tile: its group key (kept in the `cluster_id` field for
/// wire compatibility), face count, a cover face (highest-confidence detection),
/// and the identity's name when the group is a person. Biggest first.
#[derive(serde::Serialize)]
pub struct ClusterRow {
    pub cluster_id: i64,
    pub count: i64,
    pub cover_face_id: i64,
    pub name: Option<String>,
}

pub fn clusters_overview(conn: &Connection) -> Result<Vec<ClusterRow>> {
    // With exactly one MAX() aggregate, SQLite guarantees the bare columns (`id`
    // and the correlated name subquery) come from the max-score row — the cover.
    // The name is per-identity, so any row of the group yields the same value.
    let mut stmt = conn.prepare(&format!(
        "SELECT {GROUP_EXPR} AS g, COUNT(*) AS c,
                id,
                (SELECT name FROM identities i WHERE i.id = faces.identity_id),
                MAX(score)
         FROM faces
         WHERE cluster_id IS NOT NULL AND ignored = 0
         GROUP BY g
         ORDER BY c DESC"
    ))?;
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

/// The group of an existing person by name (case-insensitive), if one exists — so
/// "move to a new person: Mía" merges into the real Mía instead of minting a
/// duplicate. Only identities that still hold faces count: a ghost name left by a
/// merged-away person must not resurrect them.
pub fn group_for_name(conn: &Connection, name: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT -id FROM identities
             WHERE lower(name) = lower(?1)
               AND EXISTS (SELECT 1 FROM faces WHERE identity_id = identities.id AND ignored = 0)
             LIMIT 1",
            [name.trim()],
            |r| r.get(0),
        )
        .ok())
}

/// Name (or rename) a person-group. Empty name clears it. Naming a positive
/// (appearance) group first promotes it to a durable identity — from then on the
/// tile lives under the stable negative key, which is returned so callers holding
/// the old positive key can follow the person. Naming vouches for the group's
/// current contents: its faces become confirmed exemplars (anchor + must-links),
/// the label the magnet learns from.
pub fn name_group(conn: &Connection, group: i64, name: &str) -> Result<i64> {
    let name = name.trim();
    if name.is_empty() {
        if let Some(id) = identity_of_group(conn, group)? {
            conn.execute("UPDATE identities SET name = NULL WHERE id = ?1", [id])?;
        }
        return Ok(group);
    }
    let id = ensure_identity_for_group(conn, group)?;
    conn.execute("UPDATE identities SET name = ?1 WHERE id = ?2", rusqlite::params![name, id])?;
    confirm_identity_faces(conn, id)?;
    Ok(-id)
}

/// The display name of a group: a named identity's name. Positive (appearance)
/// groups are by definition unnamed.
pub fn group_name(conn: &Connection, group: i64) -> Result<Option<String>> {
    if group >= 0 {
        return Ok(None);
    }
    let name: Option<String> = conn
        .query_row("SELECT name FROM identities WHERE id = ?1", [-group], |r| r.get(0))
        .unwrap_or(None);
    Ok(name)
}

/// Mint a brand-new (unnamed) durable identity — the person record behind a
/// "move to a new person" split.
pub fn new_identity(conn: &Connection) -> Result<i64> {
    conn.execute("INSERT INTO identities (name) VALUES (NULL)", [])?;
    Ok(conn.last_insert_rowid())
}

/// True if the group IS a different *named* person with user-confirmed evidence —
/// absorbing or merging it away would swallow someone the user labeled. Positive
/// (appearance) groups hold no identity faces, so they never block; neither do
/// unnamed competitors (bookkeeping minted by a rejection) — the user's explicit
/// assignment outranks that bookkeeping.
pub fn group_is_other_named_person(
    conn: &Connection,
    group: i64,
    identity: Option<i64>,
) -> Result<bool> {
    if group >= 0 || identity == Some(-group) {
        return Ok(false);
    }
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM faces f JOIN identities i ON i.id = f.identity_id
         WHERE f.identity_id = ?1 AND f.confirmed = 1 AND i.name IS NOT NULL",
        [-group],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Rebind a group's faces confirmed under *unnamed* identities to `new_identity`.
/// Used when the user explicitly assigns the group to a person: the unnamed
/// competitor was minted by an earlier rejection, and without this adoption the
/// assignment would be refused forever (the stuck-card loop).
pub fn adopt_unnamed_confirmed(conn: &Connection, group: i64, new_identity: i64) -> Result<()> {
    conn.execute(
        &format!(
            "UPDATE faces SET identity_id = ?2
             WHERE {GROUP_EXPR} = ?1 AND confirmed = 1 AND identity_id != ?2
               AND identity_id IN (SELECT id FROM identities WHERE name IS NULL)"
        ),
        rusqlite::params![group, new_identity],
    )?;
    Ok(())
}

/// Mark a group's faces as user-confirmed (exemplars + must-links) under the
/// identity they're being vouched as: unclaimed faces and that identity's own.
/// A face bound to a *different* identity is untouched — confirming it here would
/// mint bogus exemplars for the other person. Used before absorbs/merges/rejects
/// so the vouched-for faces become sticky, not auto-ejectable.
pub fn confirm_group_faces(conn: &Connection, group: i64, identity: Option<i64>) -> Result<()> {
    conn.execute(
        &format!(
            "UPDATE faces SET confirmed = 1
             WHERE {GROUP_EXPR} = ?1 AND (identity_id IS NULL OR identity_id IS ?2) AND ignored = 0"
        ),
        rusqlite::params![group, identity],
    )?;
    Ok(())
}

/// Mark every face currently under an identity as user-confirmed — the "you
/// vouched for this tile's contents" primitive behind naming and merging.
pub fn confirm_identity_faces(conn: &Connection, identity: i64) -> Result<()> {
    conn.execute(
        "UPDATE faces SET confirmed = 1 WHERE identity_id = ?1 AND ignored = 0",
        [identity],
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

/// Merge a group into an identity: rebind its faces so they display under the
/// person — no cluster ids move. Rebound: unclaimed faces, faces carrying the
/// from-group's own identity (the user just vouched they're the same person),
/// and tentative machine labels. A face the user confirmed under any *other*
/// identity is never rebound (the Mía/Camila rule) — structurally rare now that
/// groups are identity-keyed, but kept as defense in depth. `confirmed` is not
/// set here; callers decide what the user actually vouched for.
///
/// If the from-side was a *named* identity that ends up empty, its name is
/// cleared — a ghost name would otherwise hijack later merge-by-name lookups.
pub fn merge_group_into_identity(conn: &Connection, into_identity: i64, from: i64) -> Result<()> {
    let from_identity = if from < 0 { Some(-from) } else { None };
    conn.execute(
        &format!(
            "UPDATE faces SET identity_id = ?1
             WHERE {GROUP_EXPR} = ?2 AND ignored = 0
               AND (identity_id IS NULL OR identity_id IS ?3 OR confirmed = 0)"
        ),
        rusqlite::params![into_identity, from, from_identity],
    )?;
    if let Some(fid) = from_identity {
        conn.execute(
            "UPDATE identities SET name = NULL
             WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM faces WHERE identity_id = ?1)",
            [fid],
        )?;
    }
    Ok(())
}

/// Tentatively assign an appearance cluster's unclaimed faces to an identity —
/// the auto-fold write. `confirmed` stays 0 so the next self-heal pass is free to
/// re-decide it, and `cluster_id` is untouched so nothing ever needs un-merging.
pub fn assign_cluster_to_identity(conn: &Connection, cluster_id: i64, identity: i64) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE faces SET identity_id = ?2
         WHERE cluster_id = ?1 AND identity_id IS NULL AND ignored = 0",
        rusqlite::params![cluster_id, identity],
    )?)
}

/// The identity behind a group key. Negative keys ARE identities; positive keys
/// are appearance clusters, which by construction hold only identity-less faces.
pub fn identity_of_group(conn: &Connection, group: i64) -> Result<Option<i64>> {
    if group >= 0 {
        return Ok(None);
    }
    let exists: bool = conn
        .query_row("SELECT 1 FROM identities WHERE id = ?1", [-group], |_| Ok(true))
        .unwrap_or(false);
    Ok(if exists { Some(-group) } else { None })
}

/// Get (or create) the durable identity behind a group. A negative key already is
/// one; a positive (appearance) key mints a fresh identity and binds the group's
/// faces to it — from then on the tile lives under the stable negative key, so
/// callers must not reuse the old positive key afterwards.
///
/// An EMPTY positive group is refused: fold passes don't bump the generation (they
/// move no ids), so a held positive key can outlive its faces — a fold may have
/// claimed them all since the UI loaded it. Minting an identity for it would bind
/// nothing, and the caller's follow-up writes (a name, a merge target) would land
/// on an invisible ghost. The error text matches the frontend's stale handling.
pub fn ensure_identity_for_group(conn: &Connection, group: i64) -> Result<i64> {
    if group < 0 {
        return Ok(-group);
    }
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM faces WHERE cluster_id = ?1 AND identity_id IS NULL AND ignored = 0",
        [group],
        |r| r.get(0),
    )?;
    if n == 0 {
        anyhow::bail!("stale group: people were reorganized since it was shown");
    }
    let id = new_identity(conn)?;
    conn.execute(
        "UPDATE faces SET identity_id = ?1
         WHERE cluster_id = ?2 AND identity_id IS NULL AND ignored = 0",
        rusqlite::params![id, group],
    )?;
    Ok(id)
}

/// (identity_id, name) for every identity that has been given a name.
pub fn named_identities(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare("SELECT id, name FROM identities WHERE name IS NOT NULL")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Record a cannot-link between two **identity** ids (a < b normalized) — the
/// durable "not the same person" barrier.
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
pub fn person_photos(conn: &Connection, group: i64) -> Result<Vec<PhotoRow>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.thumb_status, COALESCE(p.taken_ts, p.mtime) AS ts
         FROM photos p
         JOIN faces f ON f.photo_id = p.id
         WHERE COALESCE(-f.identity_id, f.cluster_id) = ?1 AND f.ignored = 0
         GROUP BY p.id
         ORDER BY ts DESC, p.id DESC",
    )?;
    let rows = stmt.query_map([group], |r| {
        Ok(PhotoRow {
            id: r.get(0)?,
            status: r.get(1)?,
            ts: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// (face_id, photo_id, ts, detector_score, embedding) for every face in a person's
/// group — the input to the person-page "looks" (intra-identity sub-clustering).
/// Ordered oldest-first so the look grouping is stable and reads chronologically.
pub fn person_faces(conn: &Connection, group: i64) -> Result<Vec<(i64, i64, i64, f32, Vec<f32>)>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.photo_id, COALESCE(p.taken_ts, p.mtime) AS ts, f.score, f.embedding
         FROM faces f
         JOIN photos p ON p.id = f.photo_id
         WHERE COALESCE(-f.identity_id, f.cluster_id) = ?1 AND f.ignored = 0
         ORDER BY ts ASC, f.id ASC",
    )?;
    let rows = stmt.query_map([group], |r| {
        let blob: Vec<u8> = r.get(4)?;
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, decode_embedding(&blob)))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------------------
// Face corrections — the shared primitive behind reassign / ignore, acting on a
// set of face ids. The person page and the in-photo overlay differ only in how
// they pick that set. Corrections write ONLY the identity layer (identity_id /
// confirmed / ignored) — never `cluster_id`, which belongs to the clustering
// passes — so they survive every re-cluster by construction.
// ---------------------------------------------------------------------------

/// A face within one photo, for the in-photo overlay: its id, bounding box, and
/// the person it currently belongs to (group key + name, if named). Ignored
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
        "SELECT f.id, COALESCE(-f.identity_id, f.cluster_id),
                (SELECT name FROM identities i WHERE i.id = f.identity_id),
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
/// the user took back. `cluster_id` is NOT captured: corrections never change the
/// appearance layer, and restoring a pre-re-cluster id would corrupt it.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct FaceState {
    pub face_id: i64,
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
        "SELECT id, identity_id, ignored, confirmed FROM faces WHERE id IN ({})",
        placeholders(face_ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(face_ids.iter()), |r| {
        Ok(FaceState {
            face_id: r.get(0)?,
            identity_id: r.get(1)?,
            ignored: r.get::<_, i64>(2)? != 0,
            confirmed: r.get::<_, i64>(3)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Undo a correction: put each face back exactly where it was.
pub fn restore_face_states(conn: &mut Connection, states: &[FaceState]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut up = tx.prepare(
            "UPDATE faces SET identity_id = ?1, ignored = ?2, confirmed = ?3 WHERE id = ?4",
        )?;
        for s in states {
            up.execute(rusqlite::params![
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

/// The face ids belonging to `group` within any of `photo_ids` — resolves a
/// person-page multi-selection (one cell per photo) to the actual faces to act on.
pub fn face_ids_in_photos_for_cluster(
    conn: &Connection,
    photo_ids: &[i64],
    group: i64,
) -> Result<Vec<i64>> {
    if photo_ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT id FROM faces WHERE {GROUP_EXPR} = ?1 AND ignored = 0 AND photo_id IN ({})",
        placeholders(photo_ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(photo_ids.len() + 1);
    params.push(&group);
    for id in photo_ids {
        params.push(id);
    }
    let rows = stmt.query_map(params.as_slice(), |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Move a set of faces onto a person: bind their durable identity and confirm
/// them in one transaction (a deliberate move is a user label — sticky exemplar
/// + must-link), clearing any `ignored` flag. The appearance `cluster_id` is
/// untouched: display follows the identity.
pub fn set_faces_person(conn: &mut Connection, face_ids: &[i64], identity_id: i64) -> Result<()> {
    if face_ids.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut up = tx.prepare(
            "UPDATE faces SET identity_id = ?1, ignored = 0, confirmed = 1 WHERE id = ?2",
        )?;
        for id in face_ids {
            up.execute(rusqlite::params![identity_id, id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Ignore a set of faces: drop them from People for good. The identity unbinds
/// and `ignored` is set; the appearance `cluster_id` is deliberately KEPT so an
/// undo restores the face to exactly its group (every group query filters
/// `ignored = 0`, so the face still leaves every grouping and the overlay).
pub fn ignore_faces(conn: &Connection, face_ids: &[i64]) -> Result<()> {
    if face_ids.is_empty() {
        return Ok(());
    }
    let sql = format!(
        "UPDATE faces SET identity_id = NULL, confirmed = 0, ignored = 1 WHERE id IN ({})",
        placeholders(face_ids.len())
    );
    conn.execute(&sql, rusqlite::params_from_iter(face_ids.iter()))?;
    Ok(())
}

/// Detach faces from their person without saying who they are: clear the identity
/// binding so each falls back to its appearance cluster, where the next self-heal
/// pass re-homes it competitively. Unlike `ignore_faces` they stay in the pool,
/// and unlike a new-person split they're not forced together. Backs "not this
/// person / not <name>".
pub fn detach_faces(conn: &Connection, face_ids: &[i64]) -> Result<()> {
    if face_ids.is_empty() {
        return Ok(());
    }
    let sql = format!(
        "UPDATE faces SET identity_id = NULL, ignored = 0, confirmed = 0 WHERE id IN ({})",
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

    fn group_of_face(conn: &Connection, face: i64) -> Option<i64> {
        conn.query_row(
            &format!("SELECT {GROUP_EXPR} FROM faces WHERE id = ?1"),
            [face],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn confirmed_of(conn: &Connection, face: i64) -> bool {
        conn.query_row("SELECT confirmed FROM faces WHERE id = ?1", [face], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
            != 0
    }

    /// Display groups: identity faces show under the (stable, negative) identity
    /// key; identity-less faces under their appearance cluster. One appearance
    /// cluster holding two people's confirmed faces therefore renders as two
    /// person tiles plus the unclaimed remainder — the Mía/Camila mixing class
    /// is structurally impossible at the display layer.
    #[test]
    fn overview_groups_by_identity_then_cluster() {
        let conn = test_conn();
        conn.execute_batch("INSERT INTO identities (id, name) VALUES (1, 'Mía'), (2, 'Camila');")
            .unwrap();
        insert_face(&conn, 1, 10, Some(1), true);
        insert_face(&conn, 2, 10, Some(1), false); // tentative fold onto Mía
        insert_face(&conn, 3, 10, Some(2), true); // Camila, same appearance cluster
        insert_face(&conn, 4, 10, None, false); // unclaimed remainder
        insert_face(&conn, 5, 20, None, false); // another pure cluster

        let rows = clusters_overview(&conn).unwrap();
        let find = |g: i64| rows.iter().find(|r| r.cluster_id == g);
        let mia = find(-1).expect("Mía's group");
        assert_eq!((mia.count, mia.name.as_deref()), (2, Some("Mía")));
        let camila = find(-2).expect("Camila's group");
        assert_eq!((camila.count, camila.name.as_deref()), (1, Some("Camila")));
        assert_eq!(find(10).expect("remainder").count, 1);
        assert_eq!(find(20).expect("pure cluster").count, 1);
    }

    /// merge_group_into_identity moves the from-group's own + tentative faces,
    /// never another person's confirmed faces, never any cluster_id — and clears
    /// the ghost name of a named identity it emptied.
    #[test]
    fn merge_transfers_group_and_clears_ghost_name() {
        let conn = test_conn();
        conn.execute_batch(
            "INSERT INTO identities (id, name) VALUES (1, 'Mía'), (2, 'Camila'), (3, 'Lianny');",
        )
        .unwrap();
        insert_face(&conn, 1, 10, Some(1), true); // Mía
        insert_face(&conn, 8, 10, Some(3), true); // Lianny, same appearance cluster
        for f in 3..=7 {
            insert_face(&conn, f, 20, Some(2), true); // Camila confirmed
        }
        insert_face(&conn, 9, 20, Some(2), false); // tentative Camila

        merge_group_into_identity(&conn, 1, -2).unwrap();

        for f in [3, 4, 5, 6, 7, 9] {
            assert_eq!(identity_of_face(&conn, f), Some(1), "face {f} should now be Mía");
        }
        assert_eq!(identity_of_face(&conn, 8), Some(3), "Lianny never stolen");
        // cluster_id untouched everywhere (appearance layer is sacred).
        let c: i64 = conn
            .query_row("SELECT cluster_id FROM faces WHERE id = 3", [], |r| r.get(0))
            .unwrap();
        assert_eq!(c, 20);
        // Camila's identity emptied → her ghost name is cleared so merge-by-name
        // can't resurrect her.
        let name: Option<String> = conn
            .query_row("SELECT name FROM identities WHERE id = 2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, None);
        assert_eq!(group_for_name(&conn, "Camila").unwrap(), None);
    }

    /// ensure_identity_for_group: a negative key passes through; a positive key
    /// mints a fresh identity and binds only the group's (identity-less) faces.
    #[test]
    fn ensure_identity_for_groups() {
        let conn = test_conn();
        conn.execute_batch("INSERT INTO identities (id, name) VALUES (1, 'Omar');").unwrap();
        insert_face(&conn, 1, 10, Some(1), true);
        insert_face(&conn, 2, 10, None, false); // remainder of the same cluster

        assert_eq!(ensure_identity_for_group(&conn, -1).unwrap(), 1);
        let minted = ensure_identity_for_group(&conn, 10).unwrap();
        assert_ne!(minted, 1, "a positive group mints a fresh identity");
        assert_eq!(identity_of_face(&conn, 2), Some(minted));
        assert_eq!(group_of_face(&conn, 2), Some(-minted), "tile moves to the stable key");
        assert_eq!(identity_of_face(&conn, 1), Some(1), "Omar's face untouched");
    }

    /// A positive group whose faces a fold claimed since the UI loaded it must be
    /// refused, not minted as a ghost: naming/merging an emptied tile should fail
    /// loudly (the frontends read this as "people were reorganized — retry").
    #[test]
    fn ensure_refuses_an_emptied_group() {
        let conn = test_conn();
        conn.execute_batch("INSERT INTO identities (id, name) VALUES (1, 'Omar');").unwrap();
        insert_face(&conn, 1, 10, Some(1), false); // the fold claimed cluster 10's face
        let err = ensure_identity_for_group(&conn, 10).unwrap_err().to_string();
        assert!(err.contains("reorganized"), "got: {err}");
        // No ghost identity was minted.
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM identities", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// group_is_other_named_person: only a named identity with confirmed evidence
    /// blocks; positive groups and unnamed competitors never do.
    #[test]
    fn other_named_person_guard() {
        let conn = test_conn();
        conn.execute_batch("INSERT INTO identities (id, name) VALUES (1, 'Omar'), (2, NULL);")
            .unwrap();
        insert_face(&conn, 1, 10, Some(1), true); // Omar, confirmed
        insert_face(&conn, 2, 20, Some(2), true); // unnamed competitor, confirmed
        insert_face(&conn, 3, 30, None, false); // pure cluster

        assert!(group_is_other_named_person(&conn, -1, Some(2)).unwrap());
        assert!(group_is_other_named_person(&conn, -1, None).unwrap());
        assert!(!group_is_other_named_person(&conn, -1, Some(1)).unwrap(), "own identity");
        assert!(!group_is_other_named_person(&conn, -2, Some(1)).unwrap(), "unnamed competitor");
        assert!(!group_is_other_named_person(&conn, 30, Some(1)).unwrap(), "pure cluster");
    }

    /// Undo must restore `confirmed` exactly. An auto-folded face (confirmed = 0)
    /// moved by the user (which confirms it) and then un-done must return to
    /// UNCONFIRMED under its old identity — leaving confirmed = 1 behind would
    /// mint a bogus user-vouched exemplar out of an action the user took back.
    #[test]
    fn undo_restores_confirmed_flag() {
        let mut conn = test_conn();
        conn.execute_batch("INSERT INTO identities (id, name) VALUES (1, 'Omar'), (2, 'Kevin');")
            .unwrap();
        insert_face(&conn, 1, 10, Some(1), false); // tentative fold onto Omar
        insert_face(&conn, 2, 10, Some(1), true); // user-confirmed Omar
        let prior = capture_face_states(&conn, &[1, 2]).unwrap();

        set_faces_person(&mut conn, &[1, 2], 2).unwrap();
        assert!(confirmed_of(&conn, 1) && confirmed_of(&conn, 2));

        restore_face_states(&mut conn, &prior).unwrap();
        assert_eq!(identity_of_face(&conn, 1), Some(1));
        assert_eq!(identity_of_face(&conn, 2), Some(1));
        assert!(!confirmed_of(&conn, 1), "tentative face must return to unconfirmed");
        assert!(confirmed_of(&conn, 2), "user-vouched face must stay confirmed");
    }

    /// Detach returns a face to its appearance cluster; ignore hides it from every
    /// group query while keeping cluster_id, so an undo restores it exactly.
    #[test]
    fn detach_and_ignore_preserve_appearance() {
        let mut conn = test_conn();
        conn.execute_batch("INSERT INTO identities (id, name) VALUES (1, 'Omar');").unwrap();
        insert_face(&conn, 1, 10, Some(1), true);

        detach_faces(&conn, &[1]).unwrap();
        assert_eq!(identity_of_face(&conn, 1), None);
        assert_eq!(group_of_face(&conn, 1), Some(10), "falls back to its appearance cluster");

        let prior = capture_face_states(&conn, &[1]).unwrap();
        ignore_faces(&conn, &[1]).unwrap();
        assert!(clusters_overview(&conn).unwrap().iter().all(|r| r.cluster_id != 10));
        assert!(top_face_ids(&conn, 10, 5).unwrap().is_empty());

        restore_face_states(&mut conn, &prior).unwrap();
        assert_eq!(group_of_face(&conn, 1), Some(10));
        assert_eq!(top_face_ids(&conn, 10, 5).unwrap(), vec![1]);
    }

    /// The same-photo exception round-trip: the pairs the rule blocks are exactly
    /// the ones a "same person (collage)" answer whitelists.
    #[test]
    fn same_photo_exception_roundtrip() {
        let conn = test_conn();
        // Photo 9 holds face 1 (group 10) and face 2 (group 20) — a collage
        // split. Photos 8/7 hold an unrelated face in each group.
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
