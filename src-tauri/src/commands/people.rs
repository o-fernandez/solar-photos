//! People & recognition commands: clusters, naming, merges, corrections and
//! their undo tokens, the review queue, and the person page's reads. Every
//! mutation that acts on a positive (appearance) cluster id verifies the
//! clustering generation under the UI connection lock (`lock_checked`).

use std::sync::atomic::Ordering;

use rusqlite::Connection;
use tauri::Manager;

use crate::recognition::{self, IdentityGrowth, PersonLook, ReviewQueue};
use crate::{cluster, db, prune_suggestion_cache, run_recluster, schedule_refold, AppState, FaceProgress};

#[tauri::command]
pub(crate) fn get_face_progress(state: tauri::State<'_, AppState>) -> Result<FaceProgress, String> {
    let conn = state.conn.lock().unwrap();
    let (scanned, eligible) = db::face_progress(&conn).map_err(|e| e.to_string())?;
    Ok(FaceProgress { scanned, eligible })
}

/// The detected people (clusters), biggest first, with a cover face each.
#[tauri::command]
pub(crate) fn get_clusters(state: tauri::State<'_, AppState>) -> Result<Vec<db::ClusterRow>, String> {
    let conn = state.conn.lock().unwrap();
    db::clusters_overview(&conn).map_err(|e| e.to_string())
}

/// What naming returns: the canonical group key to keep following (naming a
/// positive group promotes it to a durable negative key) plus the undo token.
#[derive(serde::Serialize)]
pub(crate) struct NameOutcome {
    group: i64,
    undo: CorrectionUndo,
}

/// Naming is the highest-stakes mutation — it confirms every face in the cluster
/// as user-vouched exemplars — and the cluster id was loaded from an earlier
/// people list, so it needs the same staleness guard as the suggestion paths: a
/// re-cluster between load and commit renumbers ids, and naming whatever cluster
/// now holds the stale id would durably confirm a stranger's faces under the name.
#[tauri::command]
pub(crate) fn name_cluster(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    cluster_id: i64,
    name: String,
    expected_generation: Option<i64>,
) -> Result<NameOutcome, String> {
    let outcome = {
        let conn = lock_checked(&state, expected_generation)?;
        // Captured BEFORE the write: naming confirms the group's faces (and may
        // bind them to a fresh identity), and the identity may already carry a
        // name — both must round-trip through undo.
        let prior = capture_group_states(&conn, &[cluster_id]).map_err(|e| e.to_string())?;
        let prior_name = db::group_name(&conn, cluster_id).map_err(|e| e.to_string())?;
        let group = db::name_group(&conn, cluster_id, &name).map_err(|e| e.to_string())?;
        // A negative result means an identity's name was written (or cleared);
        // a positive one means nothing happened (clearing an unnamed group).
        let renamed = (group < 0).then(|| (-group, prior_name));
        NameOutcome { group, undo: CorrectionUndo { renamed, ..CorrectionUndo::faces_only(prior) } }
    };
    // Confirming a person adds exemplars, which can re-home other people's
    // wrongly-folded look-alikes — so re-derive the folds competitively (self-heal).
    if !name.trim().is_empty() {
        schedule_refold(app);
    }
    Ok(outcome)
}

#[tauri::command]
pub(crate) fn merge_clusters(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    into: i64,
    from: i64,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let undo = {
        let conn = lock_checked(&state, expected_generation)?;
        // If exactly one side carries a name, that side survives — folding a named
        // person INTO an unnamed pile would silently un-name them.
        let (into, from) = if db::group_name(&conn, from).map_err(|e| e.to_string())?.is_some()
            && db::group_name(&conn, into).map_err(|e| e.to_string())?.is_none()
        {
            (from, into)
        } else {
            (into, from)
        };
        let prior = capture_group_states(&conn, &[into, from]).map_err(|e| e.to_string())?;
        // A user merge vouches for BOTH sides as one person — everything under the
        // surviving identity is confirmed (sticky exemplars + must-links) after the
        // fold. Confirming only one side let the next pass split the other right
        // back off, and the same "same person?" card returned — the "didn't my
        // answer register?" bug.
        let into_identity =
            db::ensure_identity_for_group(&conn, into).map_err(|e| e.to_string())?;
        db::merge_group_into_identity(&conn, into_identity, from).map_err(|e| e.to_string())?;
        db::confirm_identity_faces(&conn, into_identity).map_err(|e| e.to_string())?;
        CorrectionUndo::faces_only(prior)
    };
    prune_suggestion_cache(&state, &[into, from]);
    // The merge added exemplars — re-derive the folds competitively (self-heal).
    schedule_refold(app);
    Ok(undo)
}

/// Every photo containing this person, newest first (same ordering as the
/// timeline) — backs the person page.
#[tauri::command]
pub(crate) fn get_person_photos(
    state: tauri::State<'_, AppState>,
    cluster_id: i64,
) -> Result<Vec<db::PhotoRow>, String> {
    let conn = state.conn.lock().unwrap();
    db::person_photos(&conn, cluster_id).map_err(|e| e.to_string())
}

/// The person's "looks" strip (see `recognition::person_looks`). Runs on a
/// blocking-pool thread with its OWN connection: the leader-clustering over a big
/// person's thousands of embeddings takes real time, and computing it while
/// holding the shared UI connection stalled every avatar request behind the lock
/// (the same disease the suggestion cache cured for the People tab).
#[tauri::command]
pub(crate) async fn get_person_looks(
    state: tauri::State<'_, AppState>,
    cluster_id: i64,
) -> Result<Vec<PersonLook>, String> {
    let db_path = state.db_path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db::open(&db_path).map_err(|e| e.to_string())?;
        recognition::person_looks(&conn, cluster_id)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The faces detected in one photo, with the person each belongs to — backs the
/// in-photo overlay (name / reassign / ignore per face).
#[tauri::command]
pub(crate) fn get_faces_in_photo(
    state: tauri::State<'_, AppState>,
    photo_id: i64,
) -> Result<Vec<db::PhotoFace>, String> {
    let conn = state.conn.lock().unwrap();
    db::faces_in_photo(&conn, photo_id).map_err(|e| e.to_string())
}

/// Resolve a person-page multi-selection (photo ids + the person's cluster) to the
/// actual face ids, so the frontend can hand them to reassign/ignore.
#[tauri::command]
pub(crate) fn face_ids_for_photos(
    state: tauri::State<'_, AppState>,
    photo_ids: Vec<i64>,
    cluster_id: i64,
) -> Result<Vec<i64>, String> {
    let conn = state.conn.lock().unwrap();
    db::face_ids_in_photos_for_cluster(&conn, &photo_ids, cluster_id).map_err(|e| e.to_string())
}

/// What a correction returns so it can be undone exactly: the faces' prior state,
/// the new person's group key when one was created, any cannot-link we added, and
/// any same-photo exceptions we added (cluster-level review answers use these too).
#[derive(Clone, serde::Serialize)]
pub(crate) struct CorrectionUndo {
    prior: Vec<db::FaceState>,
    new_cluster_id: Option<i64>,
    added_cannot_link: Option<(i64, i64)>,
    /// Multi-pair form (a "neither of them" answer cannot-links against each
    /// candidate); kept alongside the single-pair field the older paths use.
    added_cannot_links: Vec<(i64, i64)>,
    added_same_photo_ok: Vec<(i64, i64)>,
    /// A name this action wrote: the identity and its name BEFORE the action
    /// (`None` = it was unnamed). Restoring face states alone would leave the
    /// label behind — and naming is what confirms faces as user-vouched, so an
    /// undo that kept it would forge exemplars out of a taken-back action.
    renamed: Option<(i64, Option<String>)>,
}

impl CorrectionUndo {
    fn faces_only(prior: Vec<db::FaceState>) -> Self {
        CorrectionUndo {
            prior,
            new_cluster_id: None,
            added_cannot_link: None,
            added_cannot_links: Vec::new(),
            added_same_photo_ok: Vec::new(),
            renamed: None,
        }
    }
}

/// Snapshot the face states of whole groups — what a cluster-level answer (merge /
/// absorb / reject / not-this-person / same-photo) needs captured for exact undo.
/// Chunked: a big person can hold thousands of faces, and SQLite caps the variables
/// one `IN (…)` may carry.
fn capture_group_states(
    conn: &rusqlite::Connection,
    groups: &[i64],
) -> anyhow::Result<Vec<db::FaceState>> {
    let mut ids: Vec<i64> = Vec::new();
    for &g in groups {
        ids.extend(db::cluster_face_ids(conn, g)?);
    }
    ids.sort_unstable();
    ids.dedup();
    let mut states = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(900) {
        states.extend(db::capture_face_states(conn, chunk)?);
    }
    Ok(states)
}

/// Reassign faces to an **existing** person (their cluster). Binds them to that
/// person's identity (must-link) and records a cannot-link from the source person,
/// so the move is durable and the two never re-merge (§4/§5 of the spec).
///
/// The generation check matters here even though face ids are stable: the *target*
/// cluster id came from a people list loaded earlier, and a re-cluster in between
/// renumbers ids — binding the faces (confirmed!) to whatever cluster now holds
/// that id would label them as the wrong person.
#[tauri::command]
pub(crate) fn reassign_faces_to_cluster(
    state: tauri::State<'_, AppState>,
    face_ids: Vec<i64>,
    source_cluster_id: i64,
    target_cluster_id: i64,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let mut conn = lock_checked(&state, expected_generation)?;
    let prior = db::capture_face_states(&conn, &face_ids).map_err(|e| e.to_string())?;
    // Both sides become durable identities; record "not the same" between them.
    let source_id =
        db::ensure_identity_for_group(&conn, source_cluster_id).map_err(|e| e.to_string())?;
    let target_id =
        db::ensure_identity_for_group(&conn, target_cluster_id).map_err(|e| e.to_string())?;
    db::set_faces_person(&mut conn, &face_ids, target_id).map_err(|e| e.to_string())?;
    let added = record_cannot_link_if_new(&conn, source_id, target_id).map_err(|e| e.to_string())?;
    Ok(CorrectionUndo { added_cannot_link: added, ..CorrectionUndo::faces_only(prior) })
}

/// Reassign faces to a **new** person (an optional name). Splits them into a fresh
/// identity + cluster and cannot-links them from the source person.
#[tauri::command]
pub(crate) fn reassign_faces_to_new_person(
    state: tauri::State<'_, AppState>,
    face_ids: Vec<i64>,
    source_cluster_id: i64,
    name: Option<String>,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let mut conn = lock_checked(&state, expected_generation)?;
    let prior = db::capture_face_states(&conn, &face_ids).map_err(|e| e.to_string())?;
    let source_id =
        db::ensure_identity_for_group(&conn, source_cluster_id).map_err(|e| e.to_string())?;
    // If the typed name is already a person, merge into them instead of minting a
    // duplicate — moving "this is someone else: Mía" twice shouldn't make two Mías.
    let trimmed = name.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(nm) = trimmed {
        if let Some(target) = db::group_for_name(&conn, nm).map_err(|e| e.to_string())? {
            if target != source_cluster_id {
                let target_id =
                    db::ensure_identity_for_group(&conn, target).map_err(|e| e.to_string())?;
                db::set_faces_person(&mut conn, &face_ids, target_id).map_err(|e| e.to_string())?;
                let added = record_cannot_link_if_new(&conn, source_id, target_id).map_err(|e| e.to_string())?;
                return Ok(CorrectionUndo { added_cannot_link: added, ..CorrectionUndo::faces_only(prior) });
            }
        }
    }
    // Mint the durable identity for the split person and bind the faces to it —
    // the new tile lives under the identity's stable (negative) group key.
    let new_id = db::new_identity(&conn).map_err(|e| e.to_string())?;
    db::set_faces_person(&mut conn, &face_ids, new_id).map_err(|e| e.to_string())?;
    if let Some(nm) = trimmed {
        let _ = db::name_group(&conn, -new_id, nm).map_err(|e| e.to_string())?;
    }
    let added = record_cannot_link_if_new(&conn, source_id, new_id).map_err(|e| e.to_string())?;
    Ok(CorrectionUndo {
        new_cluster_id: Some(-new_id),
        added_cannot_link: added,
        // The fresh identity's name (when one was given) is this action's write —
        // undo clears it so no named ghost lingers for merge-by-name lookups.
        renamed: trimmed.map(|_| (new_id, None)),
        ..CorrectionUndo::faces_only(prior)
    })
}

/// Every face in a cluster (face ids, best first) — backs the "Who is this?" split
/// grid, where the user tags each contested face as one candidate or the other and so
/// needs the whole cluster on screen, not the 3-face sample the card ships with.
#[tauri::command]
pub(crate) fn get_cluster_faces(
    state: tauri::State<'_, AppState>,
    cluster_id: i64,
) -> Result<Vec<i64>, String> {
    let conn = state.conn.lock().unwrap();
    db::cluster_face_ids(&conn, cluster_id).map_err(|e| e.to_string())
}

/// Confirm a subset of faces into an existing person, leaving the rest of their
/// current cluster untouched. Backs the "Who is this?" split: a contested cluster
/// holds two people, so the user tags some faces as A and some as B and each batch is
/// confirmed into that person. Unlike [`reassign_faces_to_cluster`] this records **no**
/// cannot-link against the source — the source is an ephemeral contested cluster, and
/// cannot-linking its untagged remainder from both people would strand faces that are
/// in fact one of them, just not tagged this round. Kicks a (review-deferred)
/// re-cluster so the remainder re-folds. Returns prior state for exact undo.
#[tauri::command]
pub(crate) fn confirm_faces_into_cluster(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    face_ids: Vec<i64>,
    target_cluster_id: i64,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    {
        let mut conn = lock_checked(&state, expected_generation)?;
        let prior = db::capture_face_states(&conn, &face_ids).map_err(|e| e.to_string())?;
        let target_id =
            db::ensure_identity_for_group(&conn, target_cluster_id).map_err(|e| e.to_string())?;
        db::set_faces_person(&mut conn, &face_ids, target_id).map_err(|e| e.to_string())?;
        drop(conn);
        schedule_refold(app);
        Ok(CorrectionUndo::faces_only(prior))
    }
}

/// Ignore faces (drop from People for good). Returns prior state for undo.
#[tauri::command]
pub(crate) fn ignore_faces(
    state: tauri::State<'_, AppState>,
    face_ids: Vec<i64>,
) -> Result<CorrectionUndo, String> {
    let conn = state.conn.lock().unwrap();
    let prior = db::capture_face_states(&conn, &face_ids).map_err(|e| e.to_string())?;
    db::ignore_faces(&conn, &face_ids).map_err(|e| e.to_string())?;
    Ok(CorrectionUndo::faces_only(prior))
}

/// "Not this person" without naming who they are: unbind the faces from their
/// current person and let the self-heal pass re-home each by appearance (possibly
/// several people, or none). Distinct from "move to a new person" (which forces
/// them together) and "ignore" (which hides them). Returns prior state for exact
/// undo — nothing but the identity layer moved.
#[tauri::command]
pub(crate) fn detach_faces(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    face_ids: Vec<i64>,
) -> Result<CorrectionUndo, String> {
    let undo = {
        let conn = state.conn.lock().unwrap();
        let prior = db::capture_face_states(&conn, &face_ids).map_err(|e| e.to_string())?;
        db::detach_faces(&conn, &face_ids).map_err(|e| e.to_string())?;
        CorrectionUndo::faces_only(prior)
    };
    schedule_refold(app);
    Ok(undo)
}

/// Undo any correction: restore the faces' prior grouping and drop any cannot-link
/// or same-photo exceptions the correction added. Re-derives the folds afterward so
/// the display reflects the restored state (deferred while a review session holds).
#[tauri::command]
pub(crate) fn undo_correction(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    undo: CorrectionUndoArg,
) -> Result<(), String> {
    {
        let mut conn = state.conn.lock().unwrap();
        db::restore_face_states(&mut conn, &undo.prior).map_err(|e| e.to_string())?;
        if let Some((a, b)) = undo.added_cannot_link {
            db::remove_cannot_link(&conn, a, b).map_err(|e| e.to_string())?;
        }
        for &(a, b) in &undo.added_cannot_links {
            db::remove_cannot_link(&conn, a, b).map_err(|e| e.to_string())?;
        }
        db::remove_same_photo_ok(&conn, &undo.added_same_photo_ok).map_err(|e| e.to_string())?;
        if let Some((identity, name)) = &undo.renamed {
            db::restore_identity_name(&conn, *identity, name.as_deref())
                .map_err(|e| e.to_string())?;
        }
    }
    schedule_refold(app);
    Ok(())
}

/// Inbound form of [`CorrectionUndo`] (the frontend hands back what a correction
/// returned). `new_cluster_id` isn't needed to undo, so it's omitted.
#[derive(serde::Deserialize)]
pub(crate) struct CorrectionUndoArg {
    prior: Vec<db::FaceState>,
    added_cannot_link: Option<(i64, i64)>,
    #[serde(default)]
    added_cannot_links: Vec<(i64, i64)>,
    #[serde(default)]
    added_same_photo_ok: Vec<(i64, i64)>,
    #[serde(default)]
    renamed: Option<(i64, Option<String>)>,
}

/// Record a cannot-link between two identities unless it already exists or they're
/// the same identity. Returns the pair when newly added (so undo can remove it).
fn record_cannot_link_if_new(
    conn: &rusqlite::Connection,
    a: i64,
    b: i64,
) -> anyhow::Result<Option<(i64, i64)>> {
    if a == b || db::cannot_link_exists(conn, a, b)? {
        return Ok(None);
    }
    db::add_cannot_link_ids(conn, a, b)?;
    Ok(Some((a, b)))
}

/// The cached growth cards from the last clustering pass. Instant — the heavy
/// pass ran in the background when clustering settled. Empty while a pass is
/// running or the cache is from an older generation (no stale cards).
#[tauri::command]
pub(crate) fn get_identity_growth(state: tauri::State<'_, AppState>) -> Result<Vec<IdentityGrowth>, String> {
    if state.reclustering.load(Ordering::SeqCst) {
        return Ok(Vec::new());
    }
    let cache = state.suggestion_cache.lock().unwrap();
    if cache.generation == state.cluster_gen.load(Ordering::SeqCst) {
        Ok(cache.growth.clone())
    } else {
        Ok(Vec::new())
    }
}

/// The unified review queue from the last clustering pass — the focus flow's feed.
#[tauri::command]
pub(crate) fn get_review_queue(state: tauri::State<'_, AppState>) -> Result<ReviewQueue, String> {
    if state.reclustering.load(Ordering::SeqCst) {
        return Ok(ReviewQueue::default());
    }
    let cache = state.suggestion_cache.lock().unwrap();
    if cache.generation == state.cluster_gen.load(Ordering::SeqCst) {
        Ok(ReviewQueue { generation: cache.generation, items: cache.queue.clone() })
    } else {
        Ok(ReviewQueue::default())
    }
}

/// Fold a batch of look-alike clusters into a confirmed person in one action (the
/// "merge all" button). Each absorb writes the durable must-link, so the whole
/// person stays together through future re-clusters.
#[tauri::command]
pub(crate) fn absorb_clusters(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    into: i64,
    clusters: Vec<i64>,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let mut touched = clusters.clone();
    touched.push(into);
    let undo = {
        let conn = lock_checked(&state, expected_generation)?;
        let prior = capture_group_states(&conn, &touched).map_err(|e| e.to_string())?;
        let into_identity =
            db::ensure_identity_for_group(&conn, into).map_err(|e| e.to_string())?;
        for from in clusters {
            if from == into {
                continue;
            }
            // Defense in depth (the suggestion pass already filters these): never
            // absorb a group that IS a different named person. Unnamed-competitor
            // confirmations are adopted instead — the user is explicitly assigning
            // this group, which outranks that bookkeeping.
            if db::group_is_other_named_person(&conn, from, Some(into_identity))
                .map_err(|e| e.to_string())?
            {
                continue;
            }
            db::adopt_unnamed_confirmed(&conn, from, into_identity).map_err(|e| e.to_string())?;
            // The user vouched for each absorbed group — confirm, then fold in.
            db::confirm_group_faces(&conn, from, Some(into_identity))
                .map_err(|e| e.to_string())?;
            db::merge_group_into_identity(&conn, into_identity, from)
                .map_err(|e| e.to_string())?;
        }
        CorrectionUndo::faces_only(prior)
    };
    prune_suggestion_cache(&state, &touched);
    // Bulk-merging added exemplars — re-derive the folds competitively (self-heal).
    schedule_refold(app);
    Ok(undo)
}

/// "Not the same" on a merge prompt: record a durable cannot-link so the pair is
/// never suggested again (survives re-clusters, unlike a dismissed-in-memory card).
/// Both sides become durable *competitors* — their faces are confirmed under their
/// (possibly unnamed) identities. Without that, the minted identity bindings were
/// unconfirmed, `clear_unconfirmed_identities` wiped them on the very next pass,
/// the cannot-link no longer matched either cluster, and the same "same person?"
/// card came straight back — rejections between unnamed groups never stuck.
#[tauri::command]
pub(crate) fn reject_merge(
    state: tauri::State<'_, AppState>,
    into: i64,
    from: i64,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let undo = {
        let conn = lock_checked(&state, expected_generation)?;
        let prior = capture_group_states(&conn, &[into, from]).map_err(|e| e.to_string())?;
        let ia = db::ensure_identity_for_group(&conn, into).map_err(|e| e.to_string())?;
        let ib = db::ensure_identity_for_group(&conn, from).map_err(|e| e.to_string())?;
        let added = record_cannot_link_if_new(&conn, ia, ib).map_err(|e| e.to_string())?;
        db::confirm_identity_faces(&conn, ia).map_err(|e| e.to_string())?;
        db::confirm_identity_faces(&conn, ib).map_err(|e| e.to_string())?;
        CorrectionUndo { added_cannot_link: added, ..CorrectionUndo::faces_only(prior) }
    };
    prune_suggestion_cache(&state, &[into, from]);
    Ok(undo)
}

/// "Not <person>" on a review candidate: instead of a weak, per-group cannot-link, make
/// the rejected group a *durable competitor* — confirm its faces as their own identity
/// (an unnamed "someone else") and cannot-link it from the person. Because confirmed
/// identities compete for faces, this generalizes: other look-alikes now get pulled
/// toward the competitor and away from the person. Re-cluster so it takes effect.
#[tauri::command]
pub(crate) fn not_this_person(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    person_cluster_id: i64,
    other_cluster_id: i64,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let undo = {
        let conn = lock_checked(&state, expected_generation)?;
        let prior = capture_group_states(&conn, &[person_cluster_id, other_cluster_id])
            .map_err(|e| e.to_string())?;
        // Mint identities for both sides + cannot-link, then confirm the rejected group
        // so it's a durable, competing exemplar (not wiped as a tentative machine label).
        let person =
            db::ensure_identity_for_group(&conn, person_cluster_id).map_err(|e| e.to_string())?;
        let other =
            db::ensure_identity_for_group(&conn, other_cluster_id).map_err(|e| e.to_string())?;
        let added = record_cannot_link_if_new(&conn, person, other).map_err(|e| e.to_string())?;
        db::confirm_identity_faces(&conn, other).map_err(|e| e.to_string())?;
        CorrectionUndo { added_cannot_link: added, ..CorrectionUndo::faces_only(prior) }
    };
    prune_suggestion_cache(&state, &[person_cluster_id, other_cluster_id]);
    schedule_refold(app);
    Ok(undo)
}

/// "Someone else" WITHOUT saying who: the contested group is none of the offered
/// candidates, and the user can't (or won't) name them right now. Cannot-link the
/// group from every candidate and confirm it as its own durable *unnamed*
/// competitor — it stops being suggested as any of them, pulls its look-alikes
/// away, and sits in People as an unnamed tile to name later (or never). The
/// answer that was missing between "it's X" and "skip forever".
#[tauri::command]
pub(crate) fn not_these_people(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    other_cluster_id: i64,
    person_cluster_ids: Vec<i64>,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let mut touched = person_cluster_ids.clone();
    touched.push(other_cluster_id);
    let undo = {
        let conn = lock_checked(&state, expected_generation)?;
        let prior = capture_group_states(&conn, &touched).map_err(|e| e.to_string())?;
        let other =
            db::ensure_identity_for_group(&conn, other_cluster_id).map_err(|e| e.to_string())?;
        let mut added = Vec::new();
        for p in &person_cluster_ids {
            let pid = db::ensure_identity_for_group(&conn, *p).map_err(|e| e.to_string())?;
            if let Some(pair) =
                record_cannot_link_if_new(&conn, other, pid).map_err(|e| e.to_string())?
            {
                added.push(pair);
            }
        }
        db::confirm_identity_faces(&conn, other).map_err(|e| e.to_string())?;
        CorrectionUndo { added_cannot_links: added, ..CorrectionUndo::faces_only(prior) }
    };
    prune_suggestion_cache(&state, &touched);
    schedule_refold(app);
    Ok(undo)
}

/// The photo behind a face crop, plus the face's normalized box — backs the
/// "peek at the full picture" affordance on review chips and cards, where a
/// tight crop alone often isn't enough to tell who someone is (the context —
/// who else is in the frame, where — is the identifying signal).
#[derive(serde::Serialize)]
pub(crate) struct FacePhoto {
    photo_id: i64,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

#[tauri::command]
pub(crate) fn get_face_photo(
    state: tauri::State<'_, AppState>,
    face_id: i64,
) -> Result<Option<FacePhoto>, String> {
    let conn = state.conn.lock().unwrap();
    Ok(db::face_box(&conn, face_id)
        .map_err(|e| e.to_string())?
        .map(|(photo_id, x1, y1, x2, y2)| FacePhoto { photo_id, x1, y1, x2, y2 }))
}

/// Name (or assign to an existing person, matched by exact name) a handful of
/// faces — WITHOUT touching the rest of their cluster and WITHOUT a cannot-link.
/// The lightbox's "just this face" scope: on a junk cluster (pose-blended
/// profiles), naming one face must not vouch for hundreds of strangers along
/// with it. The rest of the cluster re-homes competitively on later passes; the
/// named face becomes one confirmed exemplar (no magnet authority until
/// MIN_ANCHOR confirmed faces accumulate — see recognition::MIN_ANCHOR).
#[tauri::command]
pub(crate) fn name_faces(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    face_ids: Vec<i64>,
    name: String,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("a name is required".into());
    }
    let undo = {
        let mut conn = lock_checked(&state, expected_generation)?;
        let prior = db::capture_face_states(&conn, &face_ids).map_err(|e| e.to_string())?;
        // An existing person with this exact name adopts the faces; otherwise a
        // fresh identity is minted and named — and that name is this action's
        // write, so undo clears it (no named ghost for merge-by-name lookups).
        let (identity, renamed) = if let Some(group) =
            db::group_for_name(&conn, trimmed).map_err(|e| e.to_string())?
        {
            (db::ensure_identity_for_group(&conn, group).map_err(|e| e.to_string())?, None)
        } else {
            let id = db::new_identity(&conn).map_err(|e| e.to_string())?;
            let _ = db::name_group(&conn, -id, trimmed).map_err(|e| e.to_string())?;
            (id, Some((id, None)))
        };
        db::set_faces_person(&mut conn, &face_ids, identity).map_err(|e| e.to_string())?;
        CorrectionUndo {
            new_cluster_id: Some(-identity),
            renamed,
            ..CorrectionUndo::faces_only(prior)
        }
    };
    schedule_refold(app);
    Ok(undo)
}

/// "Not this person" for a whole batch of candidate groups at once — the person
/// page's review band offers "none of these are <name>". Same semantics as
/// [`not_this_person`] per group (cannot-link + durable competitor), but captured
/// as ONE undoable action, and the person's own face states are snapshotted once
/// instead of per group.
#[tauri::command]
pub(crate) fn not_this_person_many(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    person_cluster_id: i64,
    other_cluster_ids: Vec<i64>,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let mut touched = other_cluster_ids.clone();
    touched.push(person_cluster_id);
    let undo = {
        let conn = lock_checked(&state, expected_generation)?;
        let prior = capture_group_states(&conn, &touched).map_err(|e| e.to_string())?;
        let person =
            db::ensure_identity_for_group(&conn, person_cluster_id).map_err(|e| e.to_string())?;
        let mut added = Vec::new();
        for o in &other_cluster_ids {
            let oid = db::ensure_identity_for_group(&conn, *o).map_err(|e| e.to_string())?;
            if let Some(pair) =
                record_cannot_link_if_new(&conn, person, oid).map_err(|e| e.to_string())?
            {
                added.push(pair);
            }
            db::confirm_identity_faces(&conn, oid).map_err(|e| e.to_string())?;
        }
        CorrectionUndo { added_cannot_links: added, ..CorrectionUndo::faces_only(prior) }
    };
    prune_suggestion_cache(&state, &touched);
    schedule_refold(app);
    Ok(undo)
}

/// Resolve a same-photo contradiction (see [`ReviewItem::SamePhotoTwin`]).
/// `same_person = true`: it's a collage/mirror — record durable per-pair exceptions
/// for every co-occurring face pair between the two clusters, then confirm + merge
/// (the exceptions are what let the next re-cluster keep them together).
/// `same_person = false`: they're two look-alikes (twins) — durable cannot-link.
#[tauri::command]
pub(crate) fn resolve_same_photo(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    into: i64,
    from: i64,
    same_person: bool,
    expected_generation: Option<i64>,
) -> Result<CorrectionUndo, String> {
    let undo = {
        let conn = lock_checked(&state, expected_generation)?;
        let prior = capture_group_states(&conn, &[into, from]).map_err(|e| e.to_string())?;
        if same_person {
            // Resolve the blocked face pairs BEFORE any identity minting shifts
            // the positive group key out from under `cooccurring_face_pairs`.
            let pairs: Vec<(i64, i64)> = db::cooccurring_face_pairs(&conn, into, from)
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|(_, a, b)| (a, b))
                .collect();
            let into_identity =
                db::ensure_identity_for_group(&conn, into).map_err(|e| e.to_string())?;
            // Only a *named* other person blocks the assignment. Unnamed
            // competitors (minted by earlier rejections) are adopted instead —
            // refusing on them made this card unanswerable forever: every click
            // failed, the queue refreshed, and the same card came back on top.
            if db::group_is_other_named_person(&conn, from, Some(into_identity))
                .map_err(|e| e.to_string())?
            {
                return Err("that group already belongs to another named person".into());
            }
            let added_ok =
                db::add_same_photo_ok_returning_new(&conn, &pairs).map_err(|e| e.to_string())?;
            db::adopt_unnamed_confirmed(&conn, from, into_identity).map_err(|e| e.to_string())?;
            db::merge_group_into_identity(&conn, into_identity, from)
                .map_err(|e| e.to_string())?;
            // Vouch for the united person so the pairing survives self-heal.
            db::confirm_identity_faces(&conn, into_identity).map_err(|e| e.to_string())?;
            CorrectionUndo { added_same_photo_ok: added_ok, ..CorrectionUndo::faces_only(prior) }
        } else {
            // Two look-alikes: durable cannot-link, both sides durable competitors
            // (same rationale as reject_merge — unconfirmed bindings evaporate).
            let ia = db::ensure_identity_for_group(&conn, into).map_err(|e| e.to_string())?;
            let ib = db::ensure_identity_for_group(&conn, from).map_err(|e| e.to_string())?;
            let added = record_cannot_link_if_new(&conn, ia, ib).map_err(|e| e.to_string())?;
            db::confirm_identity_faces(&conn, ia).map_err(|e| e.to_string())?;
            db::confirm_identity_faces(&conn, ib).map_err(|e| e.to_string())?;
            CorrectionUndo { added_cannot_link: added, ..CorrectionUndo::faces_only(prior) }
        }
    };
    prune_suggestion_cache(&state, &[into, from]);
    schedule_refold(app);
    Ok(undo)
}

/// Fast "start people over": clear every decision (identities, names, cannot-links)
/// but keep the detected faces and their embeddings, then re-cluster from scratch,
/// unsupervised. No re-detection — seconds, not the full sweep. Snapshots the database
/// to `<db>.pre-reset.bak` first (via VACUUM INTO, a consistent copy) so a regretted
/// reset is recoverable. Returns the backup path.
#[tauri::command]
pub(crate) fn reset_face_decisions(app: tauri::AppHandle) -> Result<String, String> {
    let state = app.state::<AppState>();
    let backup = state.db_path.with_extension("pre-reset.bak");
    {
        let conn = state.conn.lock().unwrap();
        // Snapshot first (best-effort restore point), then wipe decisions.
        let _ = std::fs::remove_file(&backup);
        conn.execute("VACUUM INTO ?1", [backup.to_string_lossy().as_ref()])
            .map_err(|e| format!("backup failed: {e}"))?;
        db::clear_face_decisions(&conn).map_err(|e| e.to_string())?;
    }
    // Rebuild clusters from embeddings, unsupervised, in the background.
    run_recluster(app);
    Ok(backup.to_string_lossy().into_owned())
}

fn ensure_generation(state: &AppState, expected: Option<i64>) -> Result<(), String> {
    match expected {
        Some(g) if g != state.cluster_gen.load(Ordering::SeqCst) => {
            Err("stale suggestion: people were reorganized since it was shown".into())
        }
        _ => Ok(()),
    }
}

/// Acquire the UI connection AND verify the generation *under that lock*. The
/// re-cluster bumps the generation and commits its renumbering while holding
/// this same lock (see `run_recluster`), so a mutation either ran entirely
/// before the renumbering or sees the new generation and is refused — checking
/// before locking left a window where a stale card slipped through and wrote
/// against freshly-renumbered ids.
fn lock_checked<'a>(
    state: &'a AppState,
    expected: Option<i64>,
) -> Result<std::sync::MutexGuard<'a, Connection>, String> {
    let conn = state.conn.lock().unwrap();
    ensure_generation(state, expected)?;
    Ok(conn)
}

/// Focus-review session lifecycle: while active, due self-heal passes are deferred
/// so the session's cards stay valid; ending the session runs any deferred pass.
#[tauri::command]
pub(crate) fn set_review_active(app: tauri::AppHandle, state: tauri::State<'_, AppState>, active: bool) {
    state.review_active.store(active, Ordering::SeqCst);
    if !active && state.recluster_deferred.swap(false, Ordering::SeqCst) {
        schedule_refold(app);
    }
}

/// The current clustering generation — fetched with a people list so later
/// mutations can prove their cluster ids are from the same clustering.
#[tauri::command]
pub(crate) fn get_cluster_generation(state: tauri::State<'_, AppState>) -> i64 {
    state.cluster_gen.load(Ordering::SeqCst)
}

/// Debug-only: print the cosine distribution of mutual-kNN edges over the whole
/// face set. This is the *measurement* that sets `TAU_LINK` from a real library
/// rather than from vibes — a clean separation shows up as a trough between the
/// within-person mass (high) and the across-person tail (low); put `TAU_LINK` in
/// the trough. Returns the report as a string (also printed to the log).
#[tauri::command]
pub(crate) fn cluster_debug(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let faces = {
        let conn = state.conn.lock().unwrap();
        db::all_face_embeddings(&conn).map_err(|e| e.to_string())?
    };
    let sims = cluster::mutual_edge_sims(&faces);
    let mut report = format!(
        "cluster_debug: {} faces, {} mutual-kNN edges\n",
        faces.len(),
        sims.len()
    );
    if !sims.is_empty() {
        // 0.30..1.00 in 0.05-wide buckets — the band where TAU_LINK lives.
        let mut buckets = [0usize; 14];
        for &s in &sims {
            let b = (((s - 0.30) / 0.05).floor() as isize).clamp(0, 13) as usize;
            buckets[b] += 1;
        }
        let max = buckets.iter().copied().max().unwrap_or(1).max(1);
        for (b, &c) in buckets.iter().enumerate() {
            let lo = 0.30 + 0.05 * b as f32;
            let bar = "#".repeat(c * 40 / max);
            report.push_str(&format!("  {lo:.2}-{:.2} | {bar} {c}\n", lo + 0.05));
        }
    }
    eprintln!("{report}");
    Ok(report)
}
