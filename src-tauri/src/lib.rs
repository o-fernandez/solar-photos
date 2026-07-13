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
//!   * the **cloud** queue — a few workers, fed from two sources: the cloud-only
//!     photos the user has scrolled to (promoted to the priority lane so what's
//!     on screen always downloads first), and a slow background backfill
//!     (`spawn_cloud_backfill`) that works through the rest of the library so it
//!     eventually becomes browsable without waiting on the network. Nothing is
//!     fetched twice: every downloaded original is thumbnailed and cached forever.

mod cluster;
mod commands;
mod db;
mod faces;
mod meta;
mod prof;
mod recognition;
mod scan;
mod thumbs;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use tauri::{AppHandle, Emitter, Manager};

use db::Job;
use thumbs::ThumbQueue;
use recognition::{
    auto_fold_confident, build_review_queue, compute_identity_growth, compute_merge_suggestions,
    photo_constraints, IdentityGrowth, ReviewItem,
};

/// How many concurrent cloud downloads we allow. Bounded so a slow network
/// can't saturate the machine and so foreground work always has headroom.
const CLOUD_WORKERS: usize = 3;

/// Application-wide state, shared across command handlers and the protocol.
pub(crate) struct AppState {
    /// Path to the SQLite file (the scan thread opens its own connection here).
    pub(crate) db_path: PathBuf,
    /// Directory holding cached thumbnail JPEGs.
    pub(crate) cache_dir: PathBuf,
    /// Directory holding cached large viewer previews.
    pub(crate) preview_dir: PathBuf,
    /// Directory holding cached cover-face crops.
    pub(crate) faces_dir: PathBuf,
    /// A single connection for the (UI-driven) command handlers.
    pub(crate) conn: Mutex<Connection>,
    /// Local-file thumbnail queue (drained eagerly).
    pub(crate) local_queue: Arc<ThumbQueue>,
    /// Cloud-file queue (fed on demand with what's currently visible).
    pub(crate) cloud_queue: Arc<ThumbQueue>,
    /// Guards against two full rescans running at once (e.g. launch + manual).
    pub(crate) rescanning: Arc<AtomicBool>,
    /// Guards against two re-clusters running at once (migration + manual + sweep).
    pub(crate) reclustering: Arc<AtomicBool>,
    /// Set when a re-cluster is requested while one is already running, so the request
    /// isn't dropped — the running pass re-runs once on finish.
    pub(crate) recluster_pending: Arc<AtomicBool>,
    /// Set when a self-heal fold is requested while a fold/re-cluster is already
    /// running, so the correction that requested it still gets its re-derive.
    pub(crate) fold_pending: Arc<AtomicBool>,
    /// Monotonic clustering generation: bumped ONLY when a full re-cluster renumbers
    /// the positive (appearance) group keys. Identity groups (negative keys) are
    /// durable and never invalidated, and fold passes move no ids at all — so
    /// suggestion payloads carry the generation they were computed at, and mutations
    /// verify it (see `ensure_generation`) against genuinely rare renumbering.
    pub(crate) cluster_gen: Arc<AtomicI64>,
    /// People suggestions computed at the end of the last clustering pass (see
    /// `refresh_suggestion_cache`). The get_* commands read this instantly instead of
    /// recomputing full-library passes per tab-open while holding the DB lock.
    pub(crate) suggestion_cache: Arc<Mutex<SuggestionCache>>,
    /// Debounce token for `schedule_refold`: only the newest pending request fires.
    pub(crate) recluster_epoch: Arc<AtomicU64>,
    /// True while a focus-review session is open. The debounced self-heal pass is
    /// held during a session — it re-derives tentative folds, which would change the
    /// remaining cards' contents mid-answer; answers apply instantly either way.
    pub(crate) review_active: Arc<AtomicBool>,
    /// Set when a due self-heal pass was held by an active review session, so it
    /// runs as soon as the session ends.
    pub(crate) recluster_deferred: Arc<AtomicBool>,
}

/// The People suggestions as of one clustering generation. Served only while
/// `generation` still matches `cluster_gen` — a mismatch means clustering moved on,
/// and serving nothing beats serving cards whose cluster ids now point elsewhere.
#[derive(Default)]
pub(crate) struct SuggestionCache {
    pub(crate) generation: i64,
    pub(crate) growth: Vec<IdentityGrowth>,
    pub(crate) queue: Vec<ReviewItem>,
}

/// A monotonic-ish generation stamp for mark-and-sweep pruning.
pub(crate) fn now_gen() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Drop the calling thread — and every thread it later spawns, including ONNX
/// Runtime's inference pool — to UTILITY QoS on macOS. The webview/UI runs at a
/// higher QoS, so the scheduler always lets the foreground preempt indexing: the
/// background pools still soak up idle cores, but never at the UI's expense. This
/// is how the heavy decode + face work honors "foreground always wins"
/// (PRINCIPLES #1/#3) instead of pegging every core and freezing the window.
pub(crate) fn background_qos() {
    #[cfg(target_os = "macos")]
    {
        // QOS_CLASS_UTILITY = 0x11. `pthread_set_qos_class_self_np` lives in
        // libSystem, which is always linked, so no extra dependency is needed.
        extern "C" {
            fn pthread_set_qos_class_self_np(
                qos_class: std::os::raw::c_uint,
                relative_priority: std::os::raw::c_int,
            ) -> std::os::raw::c_int;
        }
        unsafe {
            pthread_set_qos_class_self_np(0x11, 0);
        }
    }
}

/// Remove a set of photos' cached files (thumbnail + preview) after they've been
/// pruned or their root removed.
pub(crate) fn delete_cache_files(cache_dir: &Path, preview_dir: &Path, ids: &[i64]) {
    for &id in ids {
        let _ = std::fs::remove_file(thumbs::thumb_path(cache_dir, id));
        let _ = std::fs::remove_file(thumbs::preview_path(preview_dir, id));
    }
}

/// Remove the cached cover-crop files for a set of faces. Call BEFORE deleting the
/// face rows (the crop path is keyed by face id, found via the rows) — pruned and
/// removed photos used to leave their crops on disk forever.
pub(crate) fn delete_face_crop_files(faces_dir: &Path, face_ids: &[i64]) {
    for &fid in face_ids {
        let _ = std::fs::remove_file(faces::face_crop_path(faces_dir, fid));
    }
}

/// Quietly download every cloud-only photo in the background, working through the
/// library in id order so the user doesn't have to scroll everywhere to trigger
/// on-demand fetches. Uses the same cloud queue as the on-demand path — the
/// `set_visible_range` handler promotes currently-visible photos to the priority
/// lane so they always load before the backfill, regardless of queue depth.
///
/// Backfill photos stay STATUS_CLOUD in the DB until the worker finishes and
/// sets STATUS_READY (no DOWNLOADING spinner — less noise for off-screen work).
/// When the pass reaches the end it idles for 60 s then restarts from id 0,
/// picking up any new cloud photos added by a background rescan.
fn spawn_cloud_backfill(db_path: PathBuf, cloud_queue: Arc<ThumbQueue>) {
    const BATCH: i64 = 50;
    const BATCH_PAUSE: std::time::Duration = std::time::Duration::from_secs(3);
    const IDLE_PAUSE: std::time::Duration = std::time::Duration::from_secs(60);

    std::thread::spawn(move || {
        background_qos();
        let mut after_id: i64 = 0;
        loop {
            let batch = match db::open(&db_path).and_then(|c| db::cloud_jobs_after(&c, after_id, BATCH)) {
                Ok(b) => b,
                Err(_) => {
                    std::thread::sleep(IDLE_PAUSE);
                    continue;
                }
            };
            if batch.is_empty() {
                // Reached the end of the library; reset and wait before checking again
                // (a rescan may have added new cloud photos in the meantime).
                after_id = 0;
                std::thread::sleep(IDLE_PAUSE);
            } else {
                after_id = batch.last().map(|j| j.id).unwrap_or(after_id);
                cloud_queue.enqueue(batch);
                std::thread::sleep(BATCH_PAUSE);
            }
        }
    });
}

/// Hash every LOCAL original once — the raw material for exact-duplicate
/// detection. Idle-time and resumable: `content_hash IS NULL` is the worklist,
/// so a restart continues where it left off, and new files (or newly-downloaded
/// cloud originals) get hashed on a later lap. Cloud-only placeholders are never
/// touched — reading a dataless file would force its download. An unreadable
/// file records the empty string so it's never retried (and never groups).
fn spawn_content_hasher(db_path: PathBuf) {
    const BATCH: i64 = 100;
    const IDLE_PAUSE: std::time::Duration = std::time::Duration::from_secs(120);

    std::thread::spawn(move || {
        background_qos();
        let Ok(conn) = db::open(&db_path) else { return };
        loop {
            let batch = db::unhashed_local_batch(&conn, BATCH).unwrap_or_default();
            if batch.is_empty() {
                std::thread::sleep(IDLE_PAUSE);
                continue;
            }
            for (id, path) in batch {
                let hash = hash_file(Path::new(&path)).unwrap_or_default();
                let _ = db::set_content_hash(&conn, id, &hash);
                // A gentle pace: this reads whole originals and has gigabytes to
                // get through — nothing about it is urgent (PRINCIPLES #3).
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    });
}

/// SHA-256 of a file's bytes, streamed (originals can be 50 MB HEICs).
fn hash_file(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher).ok()?;
    Some(format!("{:x}", hasher.finalize()))
}

/// How long the filesystem must stay quiet after a burst of change events
/// before the watcher reconciles. Copying a folder of 500 photos fires
/// thousands of events; coalescing them means one rescan at the end, not one
/// per file — the same "wait for the pause" shape as the refold debounce.
const WATCH_QUIET: std::time::Duration = std::time::Duration::from_secs(2);

/// Watch every library root for filesystem changes (FSEvents on macOS) and
/// reconcile automatically — new photos appear, deleted ones prune, edited
/// ones re-thumbnail, all without the user asking for a rescan. "A repeat
/// launch shows the truth" (Principle 4) extends to the running app.
///
/// One long-lived thread owns the watcher: it re-syncs its watch list with the
/// current roots once a minute (folders added or removed from the library are
/// picked up on the next sync), and turns any relevant event burst into a
/// single debounced [`rescan_all`] — which is metadata-only, overlap-guarded,
/// and refuses to prune when a root is unreachable, so a false wake is cheap
/// and a real one is exactly what the manual rescan button does.
fn spawn_fs_watcher(app: AppHandle, db_path: PathBuf) {
    use notify::Watcher;
    use std::collections::HashSet;
    use std::sync::mpsc;

    std::thread::spawn(move || {
        background_qos();
        let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = match notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        }) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("fs-watch: cannot create watcher: {e}");
                return;
            }
        };
        let mut watched: HashSet<String> = HashSet::new();
        loop {
            // Keep the watch list in sync with the library's roots.
            if let Ok(roots) = db::open(&db_path).and_then(|c| db::list_roots(&c)) {
                let roots: HashSet<String> = roots.into_iter().collect();
                for r in roots.difference(&watched) {
                    let _ = watcher.watch(Path::new(r), notify::RecursiveMode::Recursive);
                }
                for r in watched.difference(&roots) {
                    let _ = watcher.unwatch(Path::new(r));
                }
                watched = roots;
            }
            // Wait for a relevant event (the timeout doubles as the root re-sync tick).
            match rx.recv_timeout(std::time::Duration::from_secs(60)) {
                Ok(Ok(event)) if event_touches_photos(&event) => {
                    // Coalesce the burst: drain until the disk goes quiet.
                    loop {
                        match rx.recv_timeout(WATCH_QUIET) {
                            Ok(_) => continue,
                            Err(mpsc::RecvTimeoutError::Timeout) => break,
                            Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    rescan_all(app.clone());
                }
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

/// Whether a filesystem event could change what the library should contain: a
/// supported image was touched, or something structural (create/remove/rename —
/// possibly a whole folder) happened. Dot-hidden paths are ignored, matching the
/// scan's own skip rule, so sync-tool churn in `.caches` never wakes a rescan.
fn event_touches_photos(event: &notify::Event) -> bool {
    use notify::event::ModifyKind;
    use notify::EventKind;
    let structural = matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    );
    event.paths.iter().any(|p| {
        let hidden = p
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'));
        if hidden {
            return false;
        }
        // A photo changed, or a structural event on something without a photo
        // extension (most importantly: a directory moving in or out).
        scan::is_supported(p) || (structural && p.extension().is_none())
    })
}

/// Walk every root, mark what still exists, then prune what doesn't (deleted
/// files, or files under a removed root) — including their cached thumbnails.
/// This is the "second launch shows the truth" reconciliation (Principle 4). It
/// runs in the background and never blocks the UI; a guard prevents overlap.
pub(crate) fn rescan_all(app: AppHandle) {
    let state = app.state::<AppState>();
    if state.rescanning.swap(true, Ordering::SeqCst) {
        return; // a rescan is already in progress
    }
    let db_path = state.db_path.clone();
    let cache_dir = state.cache_dir.clone();
    let preview_dir = state.preview_dir.clone();
    let faces_dir = state.faces_dir.clone();
    let queue = state.local_queue.clone();
    let rescanning = state.rescanning.clone();
    drop(state);

    std::thread::spawn(move || {
        let gen = now_gen();
        let roots = match db::open(&db_path).and_then(|c| db::list_roots(&c)) {
            Ok(r) => r,
            Err(_) => {
                rescanning.store(false, Ordering::SeqCst);
                return;
            }
        };
        // Track whether every root was actually reachable. If any root is
        // missing (drive unplugged, cloud not mounted) or errored mid-walk, we
        // must NOT prune — otherwise a temporary outage would delete the library.
        let mut all_roots_ok = true;
        for root in &roots {
            if !Path::new(root).exists() {
                all_roots_ok = false;
                continue;
            }
            let app = app.clone();
            if scan::run_scan(&db_path, root, gen, queue.clone(), &preview_dir, &faces_dir, move |found, _| {
                let _ = app.emit("scan-progress", ScanProgress { found, done: false });
            })
            .is_err()
            {
                all_roots_ok = false;
            }
        }
        // Prune only when we trust the pass was complete.
        if let Ok(conn) = db::open(&db_path) {
            if all_roots_ok {
                let removed = db::take_unseen(&conn, gen).unwrap_or_default();
                // Crops first — they're found via the face rows about to go.
                let face_ids = db::face_ids_of_photos(&conn, &removed).unwrap_or_default();
                delete_face_crop_files(&faces_dir, &face_ids);
                let _ = db::delete_faces_for_photos(&conn, &removed);
                delete_cache_files(&cache_dir, &preview_dir, &removed);
            }
            let total = db::stats(&conn).map(|s| s.0).unwrap_or(0);
            let _ = app.emit("scan-progress", ScanProgress { found: total, done: true });
        }
        rescanning.store(false, Ordering::SeqCst);
    });
}

/// Progress payload for the streaming scan: how many photos are registered so
/// far, and whether the walk has finished.
#[derive(Clone, serde::Serialize)]
pub(crate) struct ScanProgress {
    pub(crate) found: i64,
    pub(crate) done: bool,
}

/// `thumb-ready` payload: which photo finished, and whether a thumbnail now
/// exists (success) or the attempt failed/was abandoned.
#[derive(Clone, serde::Serialize)]
struct ThumbDone {
    id: i64,
    ok: bool,
}

/// Progress of the background face sweep (drives the "Finding people…" readout).
#[derive(Clone, serde::Serialize)]
pub(crate) struct FaceProgress {
    pub(crate) scanned: i64,
    pub(crate) eligible: i64,
}

/// Set once the one-time migration off the old greedy clustering has run.
const RECLUSTER_FLAG: &str = "reclustered_v1";
/// Set once faces have been re-detected with the fixed alignment (see the
/// migration in `setup`). Bumping this string forces a one-time face re-sweep.
const FACES_ALIGNED_FLAG: &str = "faces_aligned_v2";
/// Set once HEIC faces have been re-detected from the correctly-oriented decode.
/// Bumping this string re-runs the orientation repair (see the migration in `setup`).
const HEIC_ORIENT_FLAG: &str = "heic_orient_v1";
/// Set once the library has been re-clustered under identity-centric grouping.
/// Before it, auto-folds physically merged clusters, so the appearance layer of an
/// existing library carries welded multi-person piles; one full re-cluster rebuilds
/// it pure (display groups are identity-keyed and unaffected throughout).
const GROUPING_FLAG: &str = "identity_grouping_v1";

/// Progress of a background re-cluster. `running` flips false when it finishes, so
/// the People view can reload exactly once (and never mid-rebuild → no reflow).
#[derive(Clone, serde::Serialize)]
struct ClusterProgress {
    running: bool,
    fraction: f32,
}

/// Recompute the suggestion caches (pairwise merges + identity growth) after a
/// clustering pass. Runs on the pass's background thread with its own connection,
/// so the UI's shared connection is never held through the matrix math (the old
/// per-tab-open compute stalled every avatar request behind that lock). The get_*
/// commands then serve instant reads. Stamps the CURRENT generation: only a full
/// re-cluster (which renumbers positive appearance keys) bumps it — a fold pass
/// changes group memberships but moves no ids, so cards stay actionable across it.
fn refresh_suggestion_cache(app: &AppHandle, conn: &Connection) {
    let state = app.state::<AppState>();
    let generation = state.cluster_gen.load(Ordering::SeqCst);
    let (mut merges, twins) = compute_merge_suggestions(conn).unwrap_or_default();
    let (mut growth, who) = compute_identity_growth(conn).unwrap_or_default();
    for s in &mut merges {
        s.generation = generation;
    }
    for g in &mut growth {
        g.generation = generation;
    }
    let mut special = who;
    special.extend(twins);
    // The pairwise merges feed the queue but aren't served on their own, so they
    // aren't cached — the queue items carry everything the review flow needs.
    let queue = build_review_queue(&merges, &growth, special);
    *state.suggestion_cache.lock().unwrap() = SuggestionCache { generation, growth, queue };
}

/// Guard for mutations holding positive (appearance) group keys: a full re-cluster
/// reassigns those from scratch, so a card computed before one completes may now
/// point at a different group of faces — acting on it would confirm/merge the wrong
/// people (durably). Identity keys (negative) never go stale, and fold passes move
/// no ids, so with identity-centric grouping this guard only ever fires around the
/// rare consolidation re-cluster. `None` is not checked.
/// Drop cached suggestions touching any of these clusters, immediately after a user
/// decision lands. The full cache only refreshes when the (debounced) clustering
/// pass completes seconds later; until then the stale cards would re-offer
/// decisions the user already made — closing and reopening Review showed the same
/// "merge all?" again, reading as "my answer didn't register." Over-pruning is
/// safe: anything still relevant is regenerated by the next pass.
pub(crate) fn prune_suggestion_cache(state: &AppState, clusters: &[i64]) {
    use std::collections::HashSet;
    let set: HashSet<i64> = clusters.iter().copied().collect();
    let touches = |item: &ReviewItem| -> bool {
        match item {
            ReviewItem::StrongBatch { into, groups, .. } => {
                set.contains(into) || groups.iter().any(|g| set.contains(&g.cluster_id))
            }
            ReviewItem::Maybe { into, group, .. } => {
                set.contains(into) || set.contains(&group.cluster_id)
            }
            ReviewItem::WhoIsThis { cluster_id, candidates, .. } => {
                set.contains(cluster_id) || candidates.iter().any(|c| set.contains(&c.into))
            }
            ReviewItem::Pairwise { into, from, .. } => set.contains(into) || set.contains(from),
            ReviewItem::SamePhotoTwin { pairs, .. } => {
                pairs.iter().any(|p| set.contains(&p.into) || set.contains(&p.from))
            }
        }
    };
    let mut cache = state.suggestion_cache.lock().unwrap();
    cache.queue.retain(|i| !touches(i));
    for g in cache.growth.iter_mut() {
        g.strong_groups.retain(|x| !set.contains(&x.cluster_id));
        g.strong_clusters.retain(|c| !set.contains(c));
        g.maybe.retain(|x| !set.contains(&x.cluster_id));
    }
    cache
        .growth
        .retain(|g| !set.contains(&g.into) && !(g.strong_clusters.is_empty() && g.maybe.is_empty()));
}

/// How long a burst of corrections may extend before the self-heal pass runs.
/// Sized for the *rhythm of reviewing*, not a single click: after accepting a few
/// suggestions on one person and returning to the grid, the user needs time to read
/// the badges and open the next person before the pass fires and "Reorganizing
/// people…" wipes the grid out from under them. A short window fired between people
/// (the "I can only check one person at a time" complaint); this lets a whole batch
/// of 5–10 people flow, coalescing into one reorganize once they actually pause.
/// The badges themselves stay correct throughout regardless — each correction prunes
/// the suggestion cache synchronously; only the deeper re-derive waits for this.
/// (While a person's page or the focus-review session is open, the pass is deferred
/// outright via `review_active`, so this window only governs idle time on the grid.)
const REFOLD_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(20);

/// Debounced [`run_auto_fold`]: a correction's DB writes apply immediately, but the
/// self-heal re-derive waits for a quiet moment, so a review session (answer,
/// answer, answer) pays for one pass instead of one per click. Each call supersedes
/// any still-pending one. This is the cheap identity-layer pass — the full
/// re-cluster now runs only when the appearance layer itself must change (sweep
/// drain, reset, migration, the manual command), never as the price of a rename.
pub(crate) fn schedule_refold(app: AppHandle) {
    let state = app.state::<AppState>();
    let epoch = state.recluster_epoch.clone();
    let review_active = state.review_active.clone();
    let deferred = state.recluster_deferred.clone();
    let mine = epoch.fetch_add(1, Ordering::SeqCst) + 1;
    drop(state);
    std::thread::spawn(move || {
        std::thread::sleep(REFOLD_DEBOUNCE);
        if epoch.load(Ordering::SeqCst) == mine {
            // Mid-review-session: hold the pass (it re-derives folds under the
            // session's remaining cards); set_review_active(false) fires it the
            // moment the session ends.
            if review_active.load(Ordering::SeqCst) {
                deferred.store(true, Ordering::SeqCst);
                return;
            }
            run_auto_fold(app);
        }
    });
}

/// Run [`auto_fold_confident`] in the background (Principle 1: off the UI thread),
/// then signal the People view to refresh via `cluster-progress`. This IS the
/// self-heal pass now: it re-derives every tentative identity assignment against
/// the current confirmed exemplars, at identity-layer cost (anchor matmuls), with
/// no re-cluster. Shares the re-cluster guard so the two never overlap; a request
/// arriving while busy is queued via `fold_pending` so corrections aren't dropped.
fn run_auto_fold(app: AppHandle) {
    let state = app.state::<AppState>();
    if state.reclustering.swap(true, Ordering::SeqCst) {
        state.fold_pending.store(true, Ordering::SeqCst);
        return;
    }
    let db_path = state.db_path.clone();
    let reclustering = state.reclustering.clone();
    let recluster_pending = state.recluster_pending.clone();
    let fold_pending = state.fold_pending.clone();
    drop(state);

    std::thread::spawn(move || {
        background_qos();
        let app_for_rerun = app.clone();
        let _ = app.emit("cluster-progress", ClusterProgress { running: true, fraction: 0.0 });
        let folded = (|| -> anyhow::Result<usize> {
            let conn = db::open(&db_path)?;
            let n = auto_fold_confident(&conn)?;
            // Memberships may have changed — recompute the suggestion caches
            // before the UI is told to reload.
            refresh_suggestion_cache(&app, &conn);
            Ok(n)
        })();
        if let Err(e) = folded {
            eprintln!("auto-fold failed: {e}");
        }
        let _ = app.emit("cluster-progress", ClusterProgress { running: false, fraction: 1.0 });
        reclustering.store(false, Ordering::SeqCst);
        // Anything queued while we ran — a full re-cluster outranks a fold (it
        // ends with one), and folds are frequent now, so dropping a queued
        // consolidation here would starve the sweep's cleanup.
        if recluster_pending.swap(false, Ordering::SeqCst) {
            run_recluster(app_for_rerun);
        } else if fold_pending.swap(false, Ordering::SeqCst) {
            run_auto_fold(app_for_rerun);
        }
    });
}

/// Re-cluster every face from scratch, in the background (Principle 1: off the UI
/// thread, no focus steal). Order-independent and purity-biased. This rebuilds the
/// *appearance* layer only: identity groups (names, confirmations, the tiles the
/// user curated) are keyed by durable identity ids and pass through untouched — no
/// name re-anchoring, no must-link welding. A guard prevents overlap; progress
/// streams via `cluster-progress`.
pub(crate) fn run_recluster(app: AppHandle) {
    let state = app.state::<AppState>();
    if state.reclustering.swap(true, Ordering::SeqCst) {
        // Already running — don't drop this request; have the running pass re-run once
        // it finishes, so a fold triggered mid-recluster (naming several people) lands.
        state.recluster_pending.store(true, Ordering::SeqCst);
        return;
    }
    let db_path = state.db_path.clone();
    let reclustering = state.reclustering.clone();
    let recluster_pending = state.recluster_pending.clone();
    let fold_pending = state.fold_pending.clone();
    recluster_pending.store(false, Ordering::SeqCst);
    let app_for_rerun = app.clone();
    drop(state);

    std::thread::spawn(move || {
        background_qos();
        let _ = app.emit("cluster-progress", ClusterProgress { running: true, fraction: 0.0 });
        let result = (|| -> anyhow::Result<()> {
            let mut conn = db::open(&db_path)?;
            let faces = db::all_face_embeddings(&conn)?;
            if faces.is_empty() {
                db::set_meta(&conn, RECLUSTER_FLAG, "1")?;
                db::set_meta(&conn, GROUPING_FLAG, "1")?;
                return Ok(());
            }

            // The user's durable "not the same person" decisions, enforced as hard
            // constraints in the agglomeration: a merge that would co-locate two
            // cannot-linked identities is refused, so embedding-close strangers
            // (e.g. two babies) the user pulled apart never drift back together.
            // Only *confirmed* (user-labeled) faces are must-links; auto-folded ones
            // are left free to re-cluster by appearance, so a wrongly-folded look-alike
            // isn't welded on — it re-homes once a better-matching person exists.
            let (photo_of, same_photo_ok) = photo_constraints(&conn)?;
            // Two identities the user gave *different* names are, by definition,
            // different people — they must never agglomerate into one cluster (which
            // would bury the smaller under the larger's name, e.g. "Mía" vanishing
            // into "Arnaldo"). Naming is the user's decision, so treat every distinct-
            // name pair as an implicit cannot-link, on top of the explicit "not the
            // same" ones. Same-name identities (one person split across two clusters)
            // are left mergeable, so this also helps re-fuse an over-split person.
            let mut cannot_link: std::collections::HashSet<(i64, i64)> =
                db::cannot_link_pairs(&conn)?.into_iter().collect();
            {
                let named = db::named_identities(&conn)?;
                for (i, (ida, na)) in named.iter().enumerate() {
                    for (idb, nb) in named.iter().skip(i + 1) {
                        if na != nb {
                            let key = if ida < idb { (*ida, *idb) } else { (*idb, *ida) };
                            cannot_link.insert(key);
                        }
                    }
                }
            }
            let constraints = cluster::LinkConstraints {
                face_identity: db::confirmed_face_identities(&conn)?.into_iter().collect(),
                cannot_link,
                photo_of,
                same_photo_ok,
            };

            // Throttle progress events to ~every 2% so we don't flood the channel.
            let app2 = app.clone();
            let mut last = 0.0f32;
            let assignments = cluster::recluster(&faces, &constraints, |f| {
                if f - last >= 0.02 || f >= 1.0 {
                    last = f;
                    let _ = app2.emit("cluster-progress", ClusterProgress { running: true, fraction: f });
                }
            });
            // Committing renumbered positive (appearance) keys is the only event
            // that can invalidate a group id the UI holds, so this is the one
            // place the generation bumps. It bumps BEFORE the commit, under the
            // UI connection's lock: every mutation re-checks the generation under
            // that same lock (`lock_checked`), so it either finished before the
            // renumbering or is refused — never both check against the old world
            // and write into the new one. Identity keys pass through untouched:
            // names, confirmations and tiles need no re-anchoring at all.
            {
                let state = app.state::<AppState>();
                let _ui = state.conn.lock().unwrap();
                state.cluster_gen.fetch_add(1, Ordering::SeqCst);
                db::set_face_clusters(&mut conn, &assignments)?;
            }
            // Re-derive the tentative folds against the fresh appearance layer
            // (see `auto_fold_confident`).
            let _ = auto_fold_confident(&conn)?;
            db::set_meta(&conn, RECLUSTER_FLAG, "1")?;
            db::set_meta(&conn, GROUPING_FLAG, "1")?;
            // Recompute the suggestion caches before the UI is told to reload.
            refresh_suggestion_cache(&app, &conn);
            Ok(())
        })();
        if let Err(e) = result {
            eprintln!("recluster failed: {e}");
        }
        let _ = app.emit("cluster-progress", ClusterProgress { running: false, fraction: 1.0 });
        reclustering.store(false, Ordering::SeqCst);
        // Anything requested while we ran — honor it now so nothing is dropped.
        // (A queued fold after a re-cluster is cheap belt-and-braces: the pass
        // above already re-derived folds, but a correction may have landed since.)
        if recluster_pending.swap(false, Ordering::SeqCst) {
            run_recluster(app_for_rerun);
        } else if fold_pending.swap(false, Ordering::SeqCst) {
            run_auto_fold(app_for_rerun);
        }
    });
}

/// Locate a bundled model: prefer the packaged resource dir, fall back to the
/// source tree for `tauri dev`.
fn resolve_model(app: &AppHandle, name: &str) -> PathBuf {
    if let Ok(p) = app
        .path()
        .resolve(format!("models/{name}"), tauri::path::BaseDirectory::Resource)
    {
        if p.exists() {
            return p;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models").join(name)
}

/// How many face workers detect + embed in parallel. The per-photo cost is
/// dominated by reading the (cloud-backed) original off ProtonDrive, which is
/// I/O-latency-bound — overlapping several reads is the main lever on sweep
/// throughput. CoreML serializes at the hardware level, so this is sized for
/// I/O overlap, not CPU. Kept modest (and UTILITY-QoS'd, see `background_qos`) so
/// the pool overlaps reads without crowding the foreground off the CPU.
const FACE_WORKERS: usize = 4;

/// The background face sweep: a pool of `FACE_WORKERS` that decode + detect +
/// embed in parallel, plus a single coordinator thread that hands out work and
/// does all the clustering and DB writes. Splitting it this way keeps the
/// expensive, parallelizable part (I/O + inference) concurrent while clustering
/// stays single-threaded and deterministic — the online cluster index is mutated
/// in exactly one place, in a well-defined order.
///
/// It only touches photos that already have a thumbnail (i.e. are local), so the
/// first pass over a cloud library lags thumbnailing, and it's resumable across
/// launches via `faces_scanned` (claims are reset on startup, see below).
fn spawn_face_workers(
    app: AppHandle,
    db_path: PathBuf,
    preview_dir: PathBuf,
    yunet: PathBuf,
    sface: PathBuf,
) {
    use std::sync::mpsc;

    // Keep ~2 jobs in flight per worker so a worker rarely waits for the
    // coordinator to refill, but a crash/quit leaves only a handful of photos in
    // the "claimed" state to recover.
    let target_outstanding = FACE_WORKERS * 2;
    let claim_batch = (FACE_WORKERS * 2) as i64;

    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let job_rx = Arc::new(Mutex::new(job_rx));
    let (res_tx, res_rx) = mpsc::channel::<(i64, Vec<db::DetectedFace>)>();

    // Worker pool: parallel decode + detect + embed. Each owns its own models.
    for _ in 0..FACE_WORKERS {
        let job_rx = job_rx.clone();
        let res_tx = res_tx.clone();
        let preview_dir = preview_dir.clone();
        let yunet = yunet.clone();
        let sface = sface.clone();
        std::thread::spawn(move || {
            // Set QoS before loading the models so ONNX Runtime's own inference
            // threads inherit UTILITY QoS and yield to the UI (PRINCIPLES #3).
            background_qos();
            let mut models = match faces::FaceModels::load(&yunet, &sface) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("faces: model load failed: {e}");
                    return;
                }
            };
            loop {
                // Block until a job is available (or the coordinator is gone).
                let job = {
                    let rx = job_rx.lock().unwrap();
                    rx.recv()
                };
                let job = match job {
                    Ok(j) => j,
                    Err(_) => break, // coordinator dropped the sender → shut down
                };
                // On any decode/inference error, fall through to an empty result —
                // the coordinator still marks the photo scanned so we never re-loop.
                let found = thumbs::load_face_source(&preview_dir, job.id, &job.path)
                    .ok()
                    .map(|img| models.process(&img).unwrap_or_default())
                    .unwrap_or_default();
                if res_tx.send((job.id, found)).is_err() {
                    break; // coordinator gone
                }
            }
        });
    }
    drop(res_tx); // only the workers hold result senders now

    // Coordinator: claims work, feeds the pool, and serially clusters + saves.
    std::thread::spawn(move || {
        background_qos();
        let mut conn = match db::open(&db_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("faces: cannot open db: {e}");
                return;
            }
        };
        // Recover any photos a previous run claimed but didn't finish.
        let _ = db::reset_claimed_faces(&conn);
        // Rebuild the cluster index from any faces clustered in a prior session.
        let mut index = cluster::ClusterIndex::load(db::clustered_embeddings(&conn).unwrap_or_default());
        let mut outstanding = 0usize;
        // Faces assigned by the (cheap, approximate) incremental path since the last
        // full consolidation. When the sweep drains we run a purity-first rebuild to
        // tidy them up — the incremental path keeps things usable in the meantime.
        let mut pending_consolidation: u64 = 0;
        loop {
            // Top up the pool's backlog.
            while outstanding < target_outstanding {
                let batch = db::claim_faces_batch(&mut conn, claim_batch).unwrap_or_default();
                if batch.is_empty() {
                    break;
                }
                for job in batch {
                    outstanding += 1;
                    if job_tx.send(job).is_err() {
                        return; // all workers gone
                    }
                }
            }
            // Drain a finished photo: assign clusters (online, incremental) and
            // persist. Time out so we periodically re-check pause / refill / idle.
            match res_rx.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok((id, found)) => {
                    let cluster_ids: Vec<i64> =
                        found.iter().map(|f| index.assign(&f.embedding, id)).collect();
                    pending_consolidation += found.len() as u64;
                    let _ = db::save_faces(&mut conn, id, &found, &cluster_ids);
                    outstanding -= 1;
                    if let Ok((scanned, eligible)) = db::face_progress(&conn) {
                        let _ = app.emit("faces-progress", FaceProgress { scanned, eligible });
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Nothing finished this tick. When the whole library is scanned,
                    // there's no backlog to claim and nothing in flight — the sweep
                    // has drained. If new faces accreted via the incremental path,
                    // consolidate once with the full batch algorithm and refresh our
                    // in-memory index so later incremental assignments don't drift.
                    // Skipped while a re-cluster (e.g. the startup migration) runs —
                    // we retry on the next drain.
                    if outstanding == 0 {
                        if pending_consolidation > 0
                            && !app.state::<AppState>().reclustering.load(Ordering::SeqCst)
                        {
                            run_recluster(app.clone());
                            // Wait for the pass — including one that got QUEUED
                            // because a fold snuck in between the check above and
                            // the call (recluster_pending) — before reloading the
                            // in-memory index; reloading early would resurrect
                            // pre-consolidation cluster ids for new faces.
                            let st = app.state::<AppState>();
                            let guard = st.reclustering.clone();
                            let queued = st.recluster_pending.clone();
                            drop(st);
                            while guard.load(Ordering::SeqCst) || queued.load(Ordering::SeqCst) {
                                std::thread::sleep(std::time::Duration::from_millis(200));
                            }
                            index = cluster::ClusterIndex::load(
                                db::clustered_embeddings(&conn).unwrap_or_default(),
                            );
                            pending_consolidation = 0;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break, // workers gone
            }
        }
    });
}

/// Serve one `thumb://` request: a cached grid thumbnail (`/<id>`), a viewer
/// preview (`/preview/<id>`), or a cover-face crop (`/face/<id>`). On a cache miss
/// this **decodes a full-resolution original** (preview / face crop), which is far
/// too heavy to run on the webview's main thread — a burst of requests (e.g. opening
/// People, which asks for a crop per person) would freeze the UI. It is therefore
/// always invoked off-thread by the asynchronous protocol handler below.
fn serve_thumb(app: &AppHandle, full: &str) -> tauri::http::Response<Vec<u8>> {
    use tauri::http::Response;
    let ok = |bytes: Vec<u8>| {
        Response::builder()
            .header("Content-Type", "image/jpeg")
            .header("Cache-Control", "no-cache")
            .body(bytes)
            .unwrap()
    };
    let not_found = || Response::builder().status(404).body(Vec::new()).unwrap();

    let is_preview = full.starts_with("preview/");
    let is_face = full.starts_with("face/");
    let id: Option<i64> = full.rsplit('/').next().and_then(|s| s.parse().ok());
    let id = match id {
        Some(id) => id,
        None => return not_found(),
    };

    let state = app.state::<AppState>();

    // Cover face for the People screen: a crisp crop from the original, generated
    // on first request and cached forever.
    if is_face {
        let out = faces::face_crop_path(&state.faces_dir, id);
        if let Ok(bytes) = std::fs::read(&out) {
            return ok(bytes);
        }
        let (photo_id, x1, y1, x2, y2) = {
            let conn = state.conn.lock().unwrap();
            match db::face_box(&conn, id).ok().flatten() {
                Some(b) => b,
                None => return not_found(),
            }
        };
        let original = {
            let conn = state.conn.lock().unwrap();
            db::path_for_id(&conn, photo_id).ok().flatten()
        };
        let img = match original.and_then(|p| thumbs::load_oriented(std::path::Path::new(&p)).ok()) {
            Some(i) => i.to_rgb8(),
            None => return not_found(),
        };
        return match faces::crop_face_jpeg(&img, (x1, y1, x2, y2)) {
            Ok(bytes) => {
                if let Some(parent) = out.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&out, &bytes);
                ok(bytes)
            }
            Err(_) => not_found(),
        };
    }

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
        // Asynchronous so the (potentially full-res-decoding) handler runs on a
        // blocking-pool thread, never the webview's main thread. A synchronous
        // handler here froze the UI ("App Not Responding") whenever a screen asked
        // for many uncached images at once — opening People decodes a face crop per
        // person. Off-thread, the event loop stays free (PRINCIPLES #1).
        .register_asynchronous_uri_scheme_protocol("thumb", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            // Path is `/<id>`, `/preview/<id>`, or `/face/<face_id>`.
            let full = request.uri().path().trim_matches('/').to_string();
            tauri::async_runtime::spawn_blocking(move || {
                responder.respond(serve_thumb(&app, &full));
            });
        })
        .setup(|app| {
            prof::init();
            // All cached state lives under the OS app-data directory — never
            // inside the user's photo folders (their originals stay pristine).
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let db_path = data_dir.join("library.db");
            let cache_dir = data_dir.join("thumbnails");
            std::fs::create_dir_all(&cache_dir)?;
            let preview_dir = data_dir.join("previews");
            std::fs::create_dir_all(&preview_dir)?;
            let faces_dir = data_dir.join("faces");
            std::fs::create_dir_all(&faces_dir)?;

            let conn = db::open(&db_path)?;
            db::init(&conn)?;

            // One-time migration for the face-alignment fix: faces detected before it
            // have collapsed embeddings (landmarks were swapped, mangling every
            // aligned crop). Embeddings can't be repaired in place — landmarks aren't
            // stored — so discard the old faces and let the sweep re-detect them
            // correctly. Cached crops are stale too; clear them.
            if db::get_meta(&conn, FACES_ALIGNED_FLAG).ok().flatten().is_none() {
                db::reset_faces_for_recompute(&conn)?;
                let _ = std::fs::remove_dir_all(&faces_dir);
                std::fs::create_dir_all(&faces_dir)?;
                db::set_meta(&conn, FACES_ALIGNED_FLAG, "1")?;
            }

            // One-time migration for the HEIC orientation fix: iPhone HEICs store
            // rotation in the container's `irot` box (not an EXIF tag), and earlier
            // builds detected faces before that transform was applied (orientation
            // handling changed across 1c955bf → dc5df8f → 6b811c9). Those faces' boxes
            // — and the crops they were embedded from — sit in the un-rotated space,
            // so boxes land off-target and the embeddings are poor. Re-detect every
            // HEIC from the current, correctly-oriented decode, dropping the stale
            // preview + crop caches (and re-queuing thumbnails) so nothing re-detects
            // on the old pixels. Skipped cleanly once done (or when there are no HEICs).
            if db::get_meta(&conn, HEIC_ORIENT_FLAG).ok().flatten().is_none() {
                let ids = db::heic_photo_ids(&conn).unwrap_or_default();
                let face_ids = db::face_ids_of_photos(&conn, &ids).unwrap_or_default();
                db::rearm_photos_for_redetect(&conn, &ids)?;
                for fid in &face_ids {
                    let _ = std::fs::remove_file(faces::face_crop_path(&faces_dir, *fid));
                }
                for id in &ids {
                    let _ = std::fs::remove_file(thumbs::preview_path(&preview_dir, *id));
                }
                db::set_meta(&conn, HEIC_ORIENT_FLAG, "1")?;
            }

            // Has the one-time migration off the old greedy clustering run yet?
            // And has the appearance layer been rebuilt pure since identity-centric
            // grouping landed (older auto-folds physically welded clusters)?
            // (Checked now, before `conn` moves into the shared state.)
            let needs_recluster = db::get_meta(&conn, RECLUSTER_FLAG).ok().flatten().is_none()
                || db::get_meta(&conn, GROUPING_FLAG).ok().flatten().is_none();

            // A download interrupted by a previous quit is no longer running;
            // reset those placeholders so they show as cloud again (and can be
            // re-fetched when next visible).
            db::set_status_many_where_downloading(&conn)?;

            // Backfill GPS for photos indexed before the Places map existed:
            // their EXIF was never checked for a fix (geo_scanned = 0). Local
            // files only — cloud originals get theirs after an on-demand
            // download, like capture dates do. EXIF header reads on a UTILITY
            // thread: a 30k-photo library takes minutes, once; on later
            // launches the worklist is empty and the thread exits immediately.
            {
                let db_path = db_path.clone();
                std::thread::spawn(move || {
                    background_qos();
                    let Ok(conn) = db::open(&db_path) else { return };
                    loop {
                        let batch = db::geo_backfill_batch(&conn, 500).unwrap_or_default();
                        if batch.is_empty() {
                            break;
                        }
                        for (id, path) in batch {
                            let gps = meta::read_exif_meta(std::path::Path::new(&path)).gps;
                            let _ = db::set_geo_scanned(&conn, id, gps);
                        }
                        // Yield between batches — this is idle-time work.
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                });
            }

            // Local pool: thumbnail decoding is CPU-bound, so we cap it at roughly
            // half the cores and leave the rest for the foreground (the workers are
            // also UTILITY-QoS'd, see `background_qos`). Together with the face pool
            // this keeps total background decode well under the core count, so the
            // UI never gets starved the way 1-per-core did.
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(2);
            let local_workers = (cores / 2).max(2);

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
                preview_dir.clone(),
                db::STATUS_FAILED,
                false,
                notify.clone(),
            );
            thumbs::spawn_workers(
                CLOUD_WORKERS,
                cloud_queue.clone(),
                db_path.clone(),
                cache_dir.clone(),
                preview_dir.clone(),
                db::STATUS_CLOUD,
                true,
                notify,
            );

            // Resume local thumbnails left pending from a previous session.
            let pending = db::pending_jobs(&conn)?;
            local_queue.enqueue(pending);

            // Background face sweep. Resolve the bundled models and start the
            // worker pool + coordinator.
            let yunet = resolve_model(app.handle(), "yunet.onnx");
            let sface = resolve_model(app.handle(), "sface.onnx");
            spawn_face_workers(
                app.handle().clone(),
                db_path.clone(),
                preview_dir.clone(),
                yunet,
                sface,
            );

            // One-time backfill: give photos missing a capture date one parsed
            // from their filename, so a cloud library (whose EXIF we can't read
            // without downloading) still gets a sensible timeline. Then nudge the
            // frontend to re-sort.
            {
                let app2 = app.handle().clone();
                let db_path2 = db_path.clone();
                std::thread::spawn(move || {
                    if let Ok(mut c) = db::open(&db_path2) {
                        let rows = db::null_date_photos(&c).unwrap_or_default();
                        let pairs: Vec<(i64, i64)> = rows
                            .into_iter()
                            .filter_map(|(id, path)| {
                                std::path::Path::new(&path)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .and_then(meta::parse_filename_date)
                                    .map(|ts| (id, ts))
                            })
                            .collect();
                        if !pairs.is_empty() {
                            let _ = db::set_taken_ts_batch(&mut c, &pairs);
                            let total = db::stats(&c).map(|s| s.0).unwrap_or(0);
                            let _ = app2.emit("scan-progress", ScanProgress { found: total, done: true });
                        }
                    }
                });
            }

            app.manage(AppState {
                db_path,
                cache_dir,
                preview_dir,
                faces_dir,
                conn: Mutex::new(conn),
                local_queue,
                cloud_queue,
                rescanning: Arc::new(AtomicBool::new(false)),
                reclustering: Arc::new(AtomicBool::new(false)),
                recluster_pending: Arc::new(AtomicBool::new(false)),
                fold_pending: Arc::new(AtomicBool::new(false)),
                cluster_gen: Arc::new(AtomicI64::new(0)),
                suggestion_cache: Arc::new(Mutex::new(SuggestionCache::default())),
                recluster_epoch: Arc::new(AtomicU64::new(0)),
                review_active: Arc::new(AtomicBool::new(false)),
                recluster_deferred: Arc::new(AtomicBool::new(false)),
            });

            // One-time migration of the existing (greedy-clustered) mess to the
            // purity-first algorithm. Runs in the background; sets a flag so it
            // happens exactly once. New libraries simply set the flag on an empty
            // pass and rely on the incremental path + periodic consolidation.
            if needs_recluster {
                run_recluster(app.handle().clone());
            } else {
                // Settled library from a prior session: reunite any look-alike fragments
                // that belong to already-confirmed people (the fold a fresh install now
                // does on naming). No-op if the library is still scanning or empty.
                run_auto_fold(app.handle().clone());
            }

            // Proactively download every cloud-only photo in the background so the
            // user doesn't have to scroll the whole library to trigger on-demand fetches.
            // Visible photos are always prioritized over backfill (see set_visible_range).
            spawn_cloud_backfill(
                app.state::<AppState>().db_path.clone(),
                app.state::<AppState>().cloud_queue.clone(),
            );

            // Auto-sync on launch: reconcile every remembered root with disk in
            // the background, so a repeat launch shows the truth (Principle 4)
            // without the user having to ask. Cached content is already on screen.
            rescan_all(app.handle().clone());

            // …and keep it true while running: watch the roots and reconcile on
            // change, so a photo copied in (or deleted) just appears (or prunes).
            spawn_fs_watcher(
                app.handle().clone(),
                app.state::<AppState>().db_path.clone(),
            );

            // Hash local originals in the background (idle-time, resumable) so
            // exact duplicates can be found and reviewed.
            spawn_content_hasher(app.state::<AppState>().db_path.clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::library::get_library_stats,
            commands::library::get_photos_range,
            commands::library::count_photos,
            commands::library::get_photo_detail,
            commands::library::add_folder,
            commands::library::rescan,
            commands::library::list_roots,
            commands::library::remove_folder,
            commands::library::set_visible_range,
            commands::people::get_face_progress,
            commands::people::get_clusters,
            commands::people::name_cluster,
            commands::people::merge_clusters,
            commands::people::get_identity_growth,
            commands::people::get_review_queue,
            commands::people::get_cluster_generation,
            commands::people::resolve_same_photo,
            commands::people::set_review_active,
            commands::people::absorb_clusters,
            commands::people::reject_merge,
            commands::people::not_this_person,
            commands::people::not_these_people,
            commands::people::not_this_person_many,
            commands::people::name_faces,
            commands::people::get_face_photo,
            commands::geo::get_geo_points,
            commands::geo::basemap_size,
            commands::geo::read_basemap_range,
            commands::curation::set_photo_favorite,
            commands::curation::set_photo_hidden,
            commands::curation::set_photos_favorite,
            commands::curation::set_photos_hidden,
            commands::curation::get_duplicate_report,
            commands::curation::export_curation,
            commands::curation::import_curation,
            commands::library::get_on_this_day,
            commands::people::reset_face_decisions,
            commands::people::cluster_debug,
            commands::people::get_person_photos,
            commands::people::get_person_looks,
            commands::people::get_faces_in_photo,
            commands::people::face_ids_for_photos,
            commands::people::get_cluster_faces,
            commands::people::confirm_faces_into_cluster,
            commands::people::reassign_faces_to_cluster,
            commands::people::reassign_faces_to_new_person,
            commands::people::ignore_faces,
            commands::people::detach_faces,
            commands::people::undo_correction
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
