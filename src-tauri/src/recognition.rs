//! The recognition engine: everything that turns clustered faces into people
//! suggestions and automatic folds. Pure "conn in, data out" — no Tauri state,
//! no event emission — so the whole engine is unit-testable against an
//! in-memory database (see the tests at the bottom). `lib.rs` keeps only the
//! thin command wrappers, the caches, and the background orchestration.
//!
//! Read `docs/faces-pipeline.md` before changing anything here: each guard in
//! this file (competitive margin, co-occurrence veto, conflict drop, anchor
//! core) exists because of a specific real-library failure documented there.

use rusqlite::Connection;

use crate::{cluster, db};

/// Intersection-over-union of two face boxes — detects double detections of one
/// face (the only case where two same-photo boxes may be the same person).
pub fn box_iou(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f32 {
    let ix = (a.2.min(b.2) - a.0.max(b.0)).max(0.0);
    let iy = (a.3.min(b.3) - a.1.max(b.1)).max(0.0);
    let inter = ix * iy;
    let area_a = (a.2 - a.0).max(0.0) * (a.3 - a.1).max(0.0);
    let area_b = (b.2 - b.0).max(0.0) * (b.3 - b.1).max(0.0);
    let union = (area_a + area_b - inter).max(1e-9);
    inter / union
}

/// Boxes overlapping at least this much are one face detected twice, not two people.
pub const DOUBLE_DETECTION_IOU: f32 = 0.4;

/// Same-photo constraint data for clustering: `face -> photo` (multi-face photos
/// only — singletons can't conflict) and the exception pairs — double detections
/// (by IoU) plus the user's "same person (collage/mirror)" answers.
pub fn photo_constraints(
    conn: &Connection,
) -> anyhow::Result<(std::collections::HashMap<i64, i64>, std::collections::HashSet<(i64, i64)>)> {
    let rows = db::multi_face_boxes(conn)?; // ordered by photo_id
    let mut photo_of = std::collections::HashMap::new();
    let mut ok: std::collections::HashSet<(i64, i64)> = db::same_photo_ok_pairs(conn)?
        .into_iter()
        .map(|(x, y)| if x < y { (x, y) } else { (y, x) })
        .collect();
    let mut i = 0;
    while i < rows.len() {
        let mut j = i;
        while j < rows.len() && rows[j].0 == rows[i].0 {
            j += 1;
        }
        for a in i..j {
            photo_of.insert(rows[a].1, rows[a].0);
            for b in (a + 1)..j {
                let ba = (rows[a].2, rows[a].3, rows[a].4, rows[a].5);
                let bb = (rows[b].2, rows[b].3, rows[b].4, rows[b].5);
                if box_iou(ba, bb) >= DOUBLE_DETECTION_IOU {
                    let (x, y) = (rows[a].1, rows[b].1);
                    ok.insert(if x < y { (x, y) } else { (y, x) });
                }
            }
        }
        i = j;
    }
    Ok((photo_of, ok))
}

/// Per-cluster and per-confirmed-identity photo sets, for the co-occurrence veto:
/// a candidate group photographed *alongside* a person cannot BE that person.
pub fn cooccurrence_maps(
    conn: &Connection,
) -> anyhow::Result<(
    std::collections::HashMap<i64, std::collections::HashSet<i64>>,
    std::collections::HashMap<i64, std::collections::HashSet<i64>>,
)> {
    let mut cluster_photos: std::collections::HashMap<i64, std::collections::HashSet<i64>> =
        std::collections::HashMap::new();
    for (cid, pid) in db::cluster_photo_pairs(conn)? {
        cluster_photos.entry(cid).or_default().insert(pid);
    }
    let mut identity_photos: std::collections::HashMap<i64, std::collections::HashSet<i64>> =
        std::collections::HashMap::new();
    for (ident, pid) in db::confirmed_identity_photos(conn)? {
        identity_photos.entry(ident).or_default().insert(pid);
    }
    Ok((cluster_photos, identity_photos))
}

/// True if the cluster shares at least one photo with the identity's confirmed
/// faces — they appear together, so they're two different people.
pub fn cooccurs(
    cluster_photos: &std::collections::HashMap<i64, std::collections::HashSet<i64>>,
    cid: i64,
    identity_photos: &std::collections::HashMap<i64, std::collections::HashSet<i64>>,
    identity: i64,
) -> bool {
    match (cluster_photos.get(&cid), identity_photos.get(&identity)) {
        (Some(cp), Some(ip)) => {
            let (small, big) = if cp.len() <= ip.len() { (cp, ip) } else { (ip, cp) };
            small.iter().any(|p| big.contains(p))
        }
        _ => false,
    }
}

/// L2-normalized mean of a set of embeddings — a robust single-vector summary of a
/// look or an identity's anchor. (Cosine of two of these is their centroid cosine.)
pub fn mean_normalized(v: &[Vec<f32>]) -> Vec<f32> {
    if v.is_empty() {
        return Vec::new();
    }
    let dim = v[0].len();
    let mut s = vec![0f32; dim];
    for e in v {
        for (k, x) in e.iter().enumerate() {
            s[k] += *x;
        }
    }
    let n = s.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    s.iter().map(|x| x / n).collect()
}

/// Cosine of two already-normalized vectors (a dot product).
pub fn cos(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// One "look" of a person on their page: a coarse appearance sub-cluster of their own
/// faces, used both to filter their photos (baby / kid / adult) and — when the look
/// actually matches a *different* named person — to move a misclassified batch out.
#[derive(Clone, serde::Serialize)]
pub struct PersonLook {
    /// Representative face (highest detector score in the look).
    pub cover_face_id: i64,
    /// Distinct photos in this look (drives the count and the grid filter).
    pub photos: i64,
    pub from_ts: i64,
    pub to_ts: i64,
    pub photo_ids: Vec<i64>,
    /// Set when the look looks more like a different named person than like this one:
    /// their name, and the cluster to move the batch into. The repair suggestion.
    pub likely_other_name: Option<String>,
    pub likely_other_cluster: Option<i64>,
}

// Look-grouping tuning. Raw leader-clustering only ("stage 1"): no centroid merge —
// within one person the look centroids sit at 0.70–0.95 cosine (measured on a real
// 5k-face person), so any merge threshold that fuses pose variants also chains every
// era into one blob that the filters then suppress, and the strip shows nothing.
// Raw fine looks with an absolute floor + cap is what actually surfaces "kid Omar".
pub const LOOK_TAU: f32 = 0.5; // leader-cluster threshold for the fine grouping
pub const LOOK_ABS_MIN: i64 = 10; // a genuine look needs at least this many photos
                              // (no relative floor: a childhood is a tiny share of a
                              // lifetime library — that's the point of the feature)
pub const MAX_LOOKS: usize = 8; // genuine looks shown, biggest first (flagged bypass)
pub const LOOK_FLAG_ABS: f32 = 0.5; // a look must match another anchor at least this well…
pub const LOOK_FLAG_MARGIN: f32 = 0.08; // …and beat its match to *this* person by this much

/// Group a person's faces into coarse "looks" for the person page: appearance-and-date
/// sub-clusters to filter by, and — where a look matches a different confirmed person
/// better than this one — a one-click "move the batch" repair. Empty (no strip) unless
/// there are at least two looks worth showing.
pub fn person_looks(conn: &Connection, group: i64) -> Result<Vec<PersonLook>, String> {
    let faces = db::person_faces(conn, group).map_err(|e| e.to_string())?;
    if faces.len() < 16 {
        return Ok(Vec::new());
    }
    let embs: Vec<Vec<f32>> = faces.iter().map(|f| f.4.clone()).collect();

    // Fine leader grouping only. No centroid-merge pass: within one person every
    // look is "similar" (they're the same face), so merging chains eras together
    // transitively and collapses the strip to a single suppressed blob.
    let groups = cluster::group_looks(&embs, LOOK_TAU);

    // This person's own reference: their anchor if they're an identity (robust to
    // a little pollution), else the dominant look's centroid.
    let own_identity = if group < 0 { Some(-group) } else { None };
    let centroids: Vec<Vec<f32>> = groups
        .iter()
        .map(|g| mean_normalized(&g.iter().map(|&i| embs[i].clone()).collect::<Vec<_>>()))
        .collect();
    let dominant = groups
        .iter()
        .enumerate()
        .max_by_key(|(_, g)| g.len())
        .map(|(i, _)| i);
    let own_ref: Option<Vec<f32>> = match own_identity {
        Some(id) => {
            let a = db::confirmed_anchor_embeddings(&conn, id, 64).map_err(|e| e.to_string())?;
            if a.is_empty() { dominant.map(|i| centroids[i].clone()) } else { Some(mean_normalized(&anchor_core(a))) }
        }
        None => dominant.map(|i| centroids[i].clone()),
    };

    // Every *other* named person we may flag a look against: enough confirmed evidence
    // to be a trustworthy target (MIN_ANCHOR), and not already declared "not the same"
    // as this person — once you've said Omar isn't Xiao Xiao, we stop suggesting it.
    let blocked: std::collections::HashSet<(i64, i64)> =
        db::cannot_link_pairs(&conn).map_err(|e| e.to_string())?.into_iter().collect();
    struct Other {
        name: String,
        cluster: i64,
        anchor: Vec<f32>,
    }
    let mut others: Vec<Other> = Vec::new();
    for (id, name) in db::named_identities(&conn).map_err(|e| e.to_string())? {
        if Some(id) == own_identity {
            continue;
        }
        if let Some(oid) = own_identity {
            let key = if oid < id { (oid, id) } else { (id, oid) };
            if blocked.contains(&key) {
                continue;
            }
        }
        let a = db::confirmed_anchor_embeddings(&conn, id, 48).map_err(|e| e.to_string())?;
        if a.len() < MIN_ANCHOR {
            continue;
        }
        // The move target is the identity's own (stable) group key.
        others.push(Other { name, cluster: -id, anchor: mean_normalized(&anchor_core(a)) });
    }

    let mut looks: Vec<PersonLook> = Vec::new();
    for (gi, g) in groups.iter().enumerate() {
        let mut photo_set = std::collections::BTreeSet::new();
        let (mut from_ts, mut to_ts) = (i64::MAX, i64::MIN);
        let mut cover = (f32::MIN, faces[g[0]].0);
        for &i in g {
            let f = &faces[i];
            photo_set.insert(f.1);
            from_ts = from_ts.min(f.2);
            to_ts = to_ts.max(f.2);
            if f.3 > cover.0 {
                cover = (f.3, f.0);
            }
        }
        // Does this look match a different confirmed person better than it matches this
        // one (by a clear margin, above an absolute bar)? Then it's likely misclassified.
        let own_sim = own_ref.as_ref().map(|r| cos(&centroids[gi], r)).unwrap_or(1.0);
        let mut flag: Option<(String, i64, f32)> = None;
        for o in &others {
            let s = cos(&centroids[gi], &o.anchor);
            if s >= LOOK_FLAG_ABS
                && s > own_sim + LOOK_FLAG_MARGIN
                && flag.as_ref().map_or(true, |(_, _, bs)| s > *bs)
            {
                flag = Some((o.name.clone(), o.cluster, s));
            }
        }
        // A genuine look shows only if it's substantial (absolute floor). A flagged
        // one shows however big or small — it's a repair prompt, not a filter.
        if flag.is_none() && (photo_set.len() as i64) < LOOK_ABS_MIN {
            continue;
        }
        looks.push(PersonLook {
            cover_face_id: cover.1,
            photos: photo_set.len() as i64,
            from_ts,
            to_ts,
            photo_ids: photo_set.into_iter().collect(),
            likely_other_name: flag.as_ref().map(|(n, _, _)| n.clone()),
            likely_other_cluster: flag.as_ref().map(|(_, c, _)| *c),
        });
    }
    // Genuine looks first (biggest first, capped so the strip stays glanceable),
    // flagged repair looks last (never capped). Only worth a strip with two or more.
    let (mut flagged, mut genuine): (Vec<PersonLook>, Vec<PersonLook>) =
        looks.into_iter().partition(|l| l.likely_other_name.is_some());
    genuine.sort_by(|a, b| b.photos.cmp(&a.photos));
    genuine.truncate(MAX_LOOKS);
    flagged.sort_by(|a, b| b.photos.cmp(&a.photos));
    genuine.extend(flagged);
    if genuine.len() < 2 {
        return Ok(Vec::new());
    }
    Ok(genuine)
}

/// A "same person?" suggestion: two clusters with several near-neighbor face pairs
/// across them (face-to-face evidence, not centroid angles). The card shows a strip
/// of example faces from each side so one glance decides.
#[derive(Clone, serde::Serialize)]
pub struct MergeSuggestion {
    pub into: i64,
    pub from: i64,
    /// Example face ids from each side (highest detector confidence), for the card.
    pub into_faces: Vec<i64>,
    pub from_faces: Vec<i64>,
    pub into_name: Option<String>,
    pub similarity: f32,
    /// Faces on the smaller side — the payoff of resolving this suggestion.
    pub photos: i64,
    /// Clustering generation this card was computed at (checked by mutations).
    pub generation: i64,
}

/// Find likely over-splits from **face-to-face** evidence: cluster pairs with at
/// least a few cross-cluster face pairs above the suggestion threshold (see
/// `cluster::merge_evidence`). Ranked by leverage — strength × combined size —
/// so the most worthwhile, most confident merges come first. The larger cluster
/// is the "into" side, so merging folds the small group into the person.
///
/// Two co-occurring clusters must look at least this alike before we raise the
/// same-photo contradiction as a question — below it they're just two people in
/// one frame (the normal case), not a suspected collage.
pub const SAME_PHOTO_ASK_MIN: f32 = 0.7;

/// Heavy (a kNN pass over every clustered face) — runs only from the background
/// cache refresh at the end of a clustering pass, never from a UI command. Empty
/// until the sweep has settled: no prompts off half-built clusters.
///
/// Also returns the same-photo contradictions (see [`ReviewItem::SamePhotoTwin`]):
/// pairs the co-occurrence rule blocks even though the faces look like one person.
/// Dropping them silently quarantined collage fragments forever; asking resolves
/// them in one glance at the shared photo.
pub fn compute_merge_suggestions(
    conn: &Connection,
) -> anyhow::Result<(Vec<MergeSuggestion>, Vec<ReviewItem>)> {
    match db::face_progress(conn)? {
        (scanned, eligible) if eligible > 0 && scanned >= eligible => {}
        _ => return Ok((Vec::new(), Vec::new())),
    }
    let overview = db::clusters_overview(conn)?;
    let faces = db::face_cluster_embeddings(conn)?;
    // Declared "not the same" identity pairs — suggestions that name them are skipped.
    let blocked: std::collections::HashSet<(i64, i64)> =
        db::cannot_link_pairs(conn)?.into_iter().collect();

    use std::collections::HashMap;
    let info: HashMap<i64, &db::ClusterRow> = overview.iter().map(|c| (c.cluster_id, c)).collect();

    let evidence = cluster::merge_evidence(&faces);
    // Rank by leverage: evidence strength × impact. Strength is the best cross-pair
    // similarity weighted by how many pairs corroborate it; impact is the combined
    // size (sqrt-damped so a few huge clusters don't crowd out confident small ones).
    let mut ranked: Vec<(cluster::PairEvidence, f32)> = evidence
        .into_iter()
        .filter_map(|e| {
            let (ca, cb) = (info.get(&e.a)?, info.get(&e.b)?);
            let leverage = e.max_sim * e.pairs as f32 * ((ca.count + cb.count) as f32).sqrt();
            Some((e, leverage))
        })
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    ranked.truncate(20);

    // Two clusters that appear in the same photo are two people — never suggest them.
    let mut cluster_photos: std::collections::HashMap<i64, std::collections::HashSet<i64>> =
        std::collections::HashMap::new();
    for (cid, pid) in db::cluster_photo_pairs(conn)? {
        cluster_photos.entry(cid).or_default().insert(pid);
    }
    let share_photo = |a: i64, b: i64| -> bool {
        match (cluster_photos.get(&a), cluster_photos.get(&b)) {
            (Some(pa), Some(pb)) => {
                let (small, big) = if pa.len() <= pb.len() { (pa, pb) } else { (pb, pa) };
                small.iter().any(|p| big.contains(p))
            }
            _ => false,
        }
    };

    let mut out = Vec::with_capacity(ranked.len());
    // Same-photo contradictions, grouped per photo: a collage split into N
    // fragments raises N cluster pairs — showing them one-per-pass looked like the
    // same broken card returning forever. One card per photo, every pair on it.
    let mut twin_pairs: std::collections::HashMap<i64, Vec<TwinPair>> =
        std::collections::HashMap::new();
    for (e, _) in ranked {
        let (big, small) = {
            let (ca, cb) = (info[&e.a], info[&e.b]);
            if ca.count >= cb.count { (ca, cb) } else { (cb, ca) }
        };
        // Merging two *named* people is only ever the explicit rename/typeahead
        // path — a one-keypress card that folds Kevin into Omar (and orphans a
        // name) must never be generated.
        if big.name.is_some() && small.name.is_some() {
            continue;
        }
        // Skip a pair the user has already declared "not the same".
        if let (Ok(Some(ia)), Ok(Some(ib))) = (
            db::identity_of_group(conn, big.cluster_id),
            db::identity_of_group(conn, small.cluster_id),
        ) {
            let key = if ia < ib { (ia, ib) } else { (ib, ia) };
            if blocked.contains(&key) {
                continue;
            }
        }
        if share_photo(e.a, e.b) {
            // The contradiction case: co-occurring, yet strong same-person evidence.
            // Never auto-merge (could be twins) — ask, showing the shared photo.
            if e.max_sim >= SAME_PHOTO_ASK_MIN {
                // A pair whose "same person" answer would be refused (the fragment
                // belongs to another *named* person) must never become a card — an
                // unanswerable question is a stuck one.
                let into_ident = db::identity_of_group(conn, big.cluster_id)?;
                if db::group_is_other_named_person(conn, small.cluster_id, into_ident)? {
                    continue;
                }
                if let Ok(pairs) = db::cooccurring_face_pairs(conn, big.cluster_id, small.cluster_id)
                {
                    if let Some(&(photo_id, fa, fb)) = pairs.first() {
                        twin_pairs.entry(photo_id).or_default().push(TwinPair {
                            into: big.cluster_id,
                            from: small.cluster_id,
                            into_name: big.name.clone(),
                            face_a: fa,
                            face_b: fb,
                            similarity: e.max_sim,
                            photos: small.count,
                        });
                    }
                }
            }
            continue;
        }
        out.push(MergeSuggestion {
            into: big.cluster_id,
            from: small.cluster_id,
            into_faces: db::top_face_ids(conn, big.cluster_id, 4).unwrap_or_default(),
            from_faces: db::top_face_ids(conn, small.cluster_id, 4).unwrap_or_default(),
            into_name: big.name.clone(),
            similarity: e.max_sim,
            photos: small.count,
            generation: 0, // stamped by refresh_suggestion_cache
        });
    }
    let twins: Vec<ReviewItem> = twin_pairs
        .into_iter()
        .map(|(photo_id, mut pairs)| {
            pairs.sort_by(|a, b| b.photos.cmp(&a.photos));
            ReviewItem::SamePhotoTwin {
                photos: pairs.iter().map(|p| p.photos).sum(),
                photo_id,
                pairs,
            }
        })
        .collect();
    Ok((out, twins))
}

/// A single less-certain growth candidate, reviewed on its own in the card's tail.
/// Carries its own example face and photo count so it renders as one yes/no chip.
#[derive(Clone, serde::Serialize)]
pub struct GrowthCluster {
    pub cluster_id: i64,
    pub face_id: Option<i64>,
    pub photos: i64,
    pub similarity: f32,
}

/// One candidate answer on a "Who is this?" card.
#[derive(Clone, serde::Serialize)]
pub struct WhoCandidate {
    pub identity_id: i64,
    pub name: String,
    /// The cluster an "it's them" answer folds the group into.
    pub into: i64,
    pub anchor_faces: Vec<i64>,
    pub similarity: f32,
}

/// One decision in the unified review queue (the focus-mode flow). Every engine's
/// output — strong batches, uncertain growth, contested clusters, pairwise
/// evidence — is normalized to this shape and sorted by payoff (photos), so the
/// user answers the biggest questions first with one grammar: yes / no / who.
#[derive(Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewItem {
    /// "N groups strongly match <name>" — bulk-mergeable, with per-group verbs.
    StrongBatch {
        photos: i64,
        name: String,
        into: i64,
        anchor_faces: Vec<i64>,
        groups: Vec<GrowthCluster>,
    },
    /// "Might also be <name>?" — one group, yes / no / someone else.
    Maybe {
        photos: i64,
        name: String,
        into: i64,
        anchor_faces: Vec<i64>,
        group: GrowthCluster,
    },
    /// Two+ named people both plausibly match this group — the near-tie the model
    /// can't resolve (babies). One answer teaches the winner *and* the loser, the
    /// highest information-per-click question in the app.
    WhoIsThis {
        photos: i64,
        cluster_id: i64,
        group_faces: Vec<i64>,
        candidates: Vec<WhoCandidate>,
    },
    /// "Same person?" — face-to-face pairwise evidence between two clusters.
    Pairwise {
        photos: i64,
        into: i64,
        from: i64,
        into_name: Option<String>,
        into_faces: Vec<i64>,
        from_faces: Vec<i64>,
    },
    /// The same-photo contradiction: clusters that look like one person (strong
    /// cross-similarity) but share a photo — one person twice (collage, mirror,
    /// booth strip) or two look-alikes (twins)? Undecidable from embeddings; the
    /// human sees the shared photo and decides in a glance. One card per photo
    /// carries every contested pair, so a collage is a single stop, not a series.
    SamePhotoTwin {
        photos: i64,
        photo_id: i64,
        pairs: Vec<TwinPair>,
    },
}

/// One contested pair on a same-photo card: the cluster the fragment would fold
/// into, and the co-occurring face from each side (cropped from the shared photo).
#[derive(Clone, serde::Serialize)]
pub struct TwinPair {
    pub into: i64,
    pub from: i64,
    pub into_name: Option<String>,
    pub face_a: i64,
    pub face_b: i64,
    pub similarity: f32,
    pub photos: i64,
}

/// The review queue as of one clustering generation. All items share the payload's
/// generation — mutations pass it back so stale answers are refused.
#[derive(Clone, serde::Serialize, Default)]
pub struct ReviewQueue {
    pub generation: i64,
    pub items: Vec<ReviewItem>,
}

/// A batch offer from a confirmed person, split by confidence. The `strong` matches
/// ("N groups are a strong match for <name>") fold in with one bulk click; the
/// less-certain `maybe` tail is reviewed one face at a time. That tail is exactly
/// where infants land — the model barely separates babies, so their look-alike
/// groups clear the linkage floor but not the strong bar — which is why the whole
/// point of the split is to keep a human glance on the risky few, not the safe many.
#[derive(Clone, serde::Serialize)]
pub struct IdentityGrowth {
    pub identity_id: i64,
    pub name: String,
    /// The group everything folds into: the identity's own stable group key.
    pub into: i64,
    /// Example faces of the confirmed person, for the card.
    pub anchor_faces: Vec<i64>,
    /// Strong matches, offered as a single bulk merge.
    pub strong_clusters: Vec<i64>,
    /// Per-group chip data for the strong matches (review-queue batch card).
    pub strong_groups: Vec<GrowthCluster>,
    /// Example faces drawn from the strong matches, for the card strip.
    pub strong_faces: Vec<i64>,
    /// Total photos across the strong matches.
    pub strong_photos: i64,
    /// The less-certain tail, each reviewed individually.
    pub maybe: Vec<GrowthCluster>,
    /// Total photos across strong + maybe (ranks the most impactful person first).
    pub photos: i64,
    /// Clustering generation this card was computed at (checked by mutations).
    pub generation: i64,
}

/// For each named person, find the over-split fragments the magnet is confident are
/// the same person (see `cluster::identity_candidates`). Anchored to the confirmed
/// identity and filtered by "not the same" — never free-chaining — so a single
/// click can reunite a person scattered across dozens of clusters. Gated on a
/// settled state, like the pairwise suggestions.
///
/// Because each identity's magnet is computed independently, the *same* look-alike
/// cluster can clear the bar against two different anchors — most visibly with
/// infants, whose embeddings the model barely separates (two babies both matching
/// each other's parent's anchor). Blanket "Merge all" would then silently hand that
/// cluster to whichever card was clicked first, writing a durable must-link. So we
/// run a two-pass conflict guard: gather every identity's candidates, then drop any
/// cluster claimed by more than one identity from *all* growth cards. Those
/// ambiguous groups aren't lost — they stay reachable through the reviewable
/// pairwise "same person?" path, where you decide one at a time.
///
/// Heavy (a full-library pass per confirmed identity) — runs only from the
/// background cache refresh, never from a UI command; the old per-tab-open compute
/// held the shared DB lock through seconds of matrix math, stalling every avatar.
///
/// Also returns the "Who is this?" review items: the clusters *dropped* from the
/// growth cards because two or more named people claim them. Those near-ties used
/// to fall silently into limbo; now they're the queue's best question.
pub fn compute_identity_growth(
    conn: &Connection,
) -> anyhow::Result<(Vec<IdentityGrowth>, Vec<ReviewItem>)> {
    match db::face_progress(conn)? {
        (scanned, eligible) if eligible > 0 && scanned >= eligible => {}
        _ => return Ok((Vec::new(), Vec::new())),
    }
    let named = db::named_identities(conn)?;
    if named.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    // Packed once and shared by the whole pass — the per-identity clones of the
    // full embedding set were its dominant allocation cost (O(people × library)).
    let all_faces = db::face_cluster_embeddings(conn)?;
    let face_matrix = cluster::FaceMatrix::new(&all_faces);
    // How well every candidate matches each confirmed identity (incl. "not X"
    // splits) — so we don't suggest a group that's decisively someone else's.
    let matches = cluster_identity_matches_with(conn, face_matrix.as_ref())?;
    // Co-occurrence veto for candidates (see auto_fold_confident).
    let (cluster_photos, identity_photos) = cooccurrence_maps(conn)?;

    // Pass 1: gather each identity's candidate clusters (already filtered by
    // "not the same"), and tally how many distinct identities claim each cluster.
    use std::collections::HashMap;
    struct Pending {
        identity_id: i64,
        name: String,
        into: i64,
        candidates: Vec<(i64, i64, f32)>, // strongest-first (cluster_id, size, max_sim)
    }
    let mut pending: Vec<Pending> = Vec::new();
    // cluster_id -> the identities claiming it (with match strength) + its size.
    let mut claims: HashMap<i64, Vec<(i64, f32)>> = HashMap::new();
    let mut claim_size: HashMap<i64, i64> = HashMap::new();
    for (identity_id, name) in named {
        let anchor = db::confirmed_anchor_embeddings(conn, identity_id, 64)?;
        if anchor.len() < MIN_ANCHOR {
            continue; // too little confirmed evidence to suggest look-alikes yet
        }
        // Match against the anchor's dominant core, not a possibly-polluted full set.
        let core = anchor_core(anchor);
        // The fold-in target is the identity's own (stable) group key; its own
        // faces are masked out of the candidate search by that same key.
        let into = -identity_id;
        let mut cands = match &face_matrix {
            Some(fm) => fm.identity_candidates(&core, Some(into)),
            None => Vec::new(),
        };
        // Strongest matches first, and drop any cluster the user said isn't this person.
        cands.sort_by(|a, b| b.max_sim.partial_cmp(&a.max_sim).unwrap());
        let mut candidates = Vec::new();
        for c in cands {
            // A negative group IS a person (or a "not X" competitor): identities
            // merge only by explicit user action, never via a growth card. This
            // also subsumes the old cannot-link check — a rejected group is a
            // confirmed competitor, i.e. a negative group, and lands here.
            if c.cluster_id < 0 {
                continue;
            }
            if cooccurs(&cluster_photos, c.cluster_id, &identity_photos, identity_id) {
                continue; // photographed together — cannot be this person
            }
            // Competitive: skip a cluster a confirmed competitor matches decisively
            // better — it's someone else's, so don't keep offering it as this person.
            if let Some(ms) = matches.get(&c.cluster_id) {
                let best_other = ms
                    .iter()
                    .filter(|(id, _)| *id != identity_id)
                    .map(|(_, s)| *s)
                    .fold(f32::MIN, f32::max);
                if best_other > c.max_sim + AUTO_FOLD_MARGIN {
                    continue;
                }
            }
            claims.entry(c.cluster_id).or_default().push((identity_id, c.max_sim));
            claim_size.insert(c.cluster_id, c.size as i64);
            candidates.push((c.cluster_id, c.size as i64, c.max_sim));
        }
        if candidates.is_empty() {
            continue;
        }
        pending.push(Pending { identity_id, name, into, candidates });
    }

    // Pass 2: build the cards, excluding any cluster claimed by 2+ identities, and
    // split the survivors by confidence. Above STRONG the match is folded in by the
    // bulk button; below it (but still past the linkage floor `identity_candidates`
    // enforced) the cluster goes to the reviewable tail, ranked by payoff and capped
    // so the chip row stays glanceable.
    const STRONG: f32 = 0.6;
    const MAX_MAYBE: usize = 12;
    // Identity display info for the who-is-this cards, captured before pass 2
    // consumes `pending`.
    let ident_info: HashMap<i64, (String, i64)> =
        pending.iter().map(|p| (p.identity_id, (p.name.clone(), p.into))).collect();
    let mut out = Vec::new();
    for p in pending {
        let mut strong_clusters = Vec::new();
        let mut strong_groups: Vec<GrowthCluster> = Vec::new();
        let mut strong_faces = Vec::new();
        let mut strong_photos: i64 = 0;
        let mut maybe: Vec<GrowthCluster> = Vec::new();
        let mut photos: i64 = 0;
        for (cid, size, sim) in p.candidates {
            if claims.get(&cid).map_or(0, |v| v.len()) > 1 {
                continue; // contested between people — becomes a who-is-this card
            }
            photos += size;
            if sim >= STRONG {
                strong_clusters.push(cid);
                let face_id = db::top_face_ids(conn, cid, 1).ok().and_then(|v| v.into_iter().next());
                strong_groups.push(GrowthCluster { cluster_id: cid, face_id, photos: size, similarity: sim });
                strong_photos += size;
                if strong_faces.len() < 4 {
                    if let Some(f) = face_id {
                        strong_faces.push(f);
                    }
                }
            } else {
                let face_id = db::top_face_ids(conn, cid, 1).ok().and_then(|v| v.into_iter().next());
                maybe.push(GrowthCluster { cluster_id: cid, face_id, photos: size, similarity: sim });
            }
        }
        if strong_clusters.is_empty() && maybe.is_empty() {
            continue;
        }
        // The review tail is ranked by payoff (photos), not similarity — a glance
        // costs the same for a 1-photo fragment as for a 40-photo group, and
        // similarity-ordering let high-sim singletons crowd big clusters out of
        // the cap (the "twelve 1-photo chips" screenshot). Cap after sorting so
        // the biggest candidates always make the strip.
        maybe.sort_by(|a, b| b.photos.cmp(&a.photos));
        maybe.truncate(MAX_MAYBE);
        out.push(IdentityGrowth {
            identity_id: p.identity_id,
            name: p.name,
            into: p.into,
            anchor_faces: db::top_face_ids(conn, p.into, 4).unwrap_or_default(),
            strong_clusters,
            strong_groups,
            strong_faces,
            strong_photos,
            maybe,
            photos,
            generation: 0, // stamped by refresh_suggestion_cache
        });
    }
    // Most impactful person first.
    out.sort_by(|a, b| b.photos.cmp(&a.photos));

    // The contested clusters (claimed by 2+ named people) become who-is-this cards.
    let mut who: Vec<ReviewItem> = Vec::new();
    for (cid, claimants) in &claims {
        if claimants.len() < 2 {
            continue;
        }
        let mut cands: Vec<WhoCandidate> = claimants
            .iter()
            .filter_map(|(id, sim)| {
                ident_info.get(id).map(|(name, into)| WhoCandidate {
                    identity_id: *id,
                    name: name.clone(),
                    into: *into,
                    anchor_faces: db::top_face_ids(conn, *into, 2).unwrap_or_default(),
                    similarity: *sim,
                })
            })
            .collect();
        if cands.len() < 2 {
            continue;
        }
        cands.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap());
        cands.truncate(3);
        who.push(ReviewItem::WhoIsThis {
            photos: claim_size.get(cid).copied().unwrap_or(0),
            cluster_id: *cid,
            group_faces: db::top_face_ids(conn, *cid, 3).unwrap_or_default(),
            candidates: cands,
        });
    }
    Ok((out, who))
}

/// Normalize every engine's suggestions into the single payoff-sorted review queue
/// the focus flow walks: strong batches and uncertain growth per person, contested
/// who-is-this clusters, and pairwise same-person evidence — biggest photos first,
/// capped so a session has a visible end.
/// Focus review is for HIGH-LEVERAGE decisions: a card must move at least this
/// many photos to earn a spot in the session. Every answer costs the same
/// attention, so sixty 1-photo "who is this?" cards read as manual labor for no
/// visible payoff. Small questions aren't lost — they stay reachable in context
/// (the person page's review band with its bulk answers, the unnamed tiles in
/// People) and many resolve themselves in later self-heal passes as confirmed
/// evidence accumulates.
pub const REVIEW_MIN_PHOTOS: i64 = 4;

pub fn build_review_queue(
    merges: &[MergeSuggestion],
    growth: &[IdentityGrowth],
    who: Vec<ReviewItem>,
) -> Vec<ReviewItem> {
    const MAX_QUEUE: usize = 60;
    let mut items = who;
    for g in growth {
        if !g.strong_groups.is_empty() {
            items.push(ReviewItem::StrongBatch {
                photos: g.strong_photos,
                name: g.name.clone(),
                into: g.into,
                anchor_faces: g.anchor_faces.clone(),
                groups: g.strong_groups.clone(),
            });
        }
        for m in &g.maybe {
            items.push(ReviewItem::Maybe {
                photos: m.photos,
                name: g.name.clone(),
                into: g.into,
                anchor_faces: g.anchor_faces.clone(),
                group: m.clone(),
            });
        }
    }
    for s in merges {
        items.push(ReviewItem::Pairwise {
            photos: s.photos,
            into: s.into,
            from: s.from,
            into_name: s.into_name.clone(),
            into_faces: s.into_faces.clone(),
            from_faces: s.from_faces.clone(),
        });
    }
    let photos_of = |i: &ReviewItem| match i {
        ReviewItem::StrongBatch { photos, .. }
        | ReviewItem::Maybe { photos, .. }
        | ReviewItem::WhoIsThis { photos, .. }
        | ReviewItem::Pairwise { photos, .. }
        | ReviewItem::SamePhotoTwin { photos, .. } => *photos,
    };
    items.retain(|i| photos_of(i) >= REVIEW_MIN_PHOTOS);
    items.sort_by(|a, b| photos_of(b).cmp(&photos_of(a)));
    items.truncate(MAX_QUEUE);
    items
}

/// The evidence floor: an identity earns "magnet authority" — the right to auto-fold
/// look-alikes in, to generate "N groups might also be…" suggestions, and to be a
/// "looks like X" flag target — only once it has at least this many *confirmed* faces.
/// One face (worse, a profile shot) defines a point, not a person; extrapolating a
/// whole identity from it is what pulls in swarms of unrelated pose/lighting matches.
/// Naming a real cluster clears this instantly; naming a single stray face does not,
/// until you confirm a few more. (`confirmed_anchor_embeddings` returns min(faces, N),
/// so `anchor.len()` is a direct read of confirmed evidence.)
pub const MIN_ANCHOR: usize = 4;

/// Minimum confirmed faces for an identity to *compete* (pull look-alikes toward it).
/// Lower than [`MIN_ANCHOR`] on purpose: a competitor can only ever push a face into
/// *review* (never silently claim it — that still needs `MIN_ANCHOR`), so it's safe to
/// let even a one-group "not Mía" rejection start defending its faces immediately.
pub const COMPETITOR_MIN: usize = 1;

/// How similar a candidate must be to a confirmed anchor before auto-fold reunites it
/// *without asking*. Above this, the match is safe to apply silently; below it (down to
/// the linkage floor) the match is real but uncertain — it goes to the review path
/// instead of being folded. Adults cluster tight and clear this easily, so they still
/// auto-reunite; two different babies rarely clear it against each other, so naming one
/// baby no longer vacuums up the others — those land in review, where a human decides.
pub const AUTO_FOLD_MIN: f32 = 0.6;

/// How much the best-matching person must beat the runner-up before a cluster is
/// auto-assigned. Below this the match is a near-tie — two people the model can't
/// separate (two babies) — so it's left for the human to resolve, never guessed.
pub const AUTO_FOLD_MARGIN: f32 = 0.06;

/// Similarity used to find an anchor's dominant appearance when trimming it to a core.
pub const ANCHOR_CORE_TAU: f32 = 0.5;

/// A cleaned "core" of an identity's anchor: cluster the exemplars and keep only the
/// dominant appearance group, so a few outliers — a wrong fold, an off-angle shot —
/// can't drag the anchor off the person and cascade (one bad fold poisoning every
/// future match). Falls back to the full set when there's no clear majority to trust,
/// or too few exemplars to bother.
pub fn anchor_core(embs: Vec<Vec<f32>>) -> Vec<Vec<f32>> {
    if embs.len() < 6 {
        return embs;
    }
    let groups = cluster::group_looks(&embs, ANCHOR_CORE_TAU);
    if let Some(biggest) = groups.iter().max_by_key(|g| g.len()) {
        // Only trust a core when it's a clear majority of the exemplars.
        if biggest.len() * 2 >= embs.len() {
            return biggest.iter().map(|&i| embs[i].clone()).collect();
        }
    }
    embs
}

/// Fold every cluster that confidently matches a *confirmed* identity's anchor into
/// that identity — the automatic reunification behind "you named them, so we gather
/// their scattered fragments for you." This is safe where unsupervised clustering
/// isn't, for the same reasons the growth prompt relied on: every match is to a
/// human-confirmed anchor (never chained cluster→cluster), covers a majority of the
/// candidate cluster, is conflict-guarded (a fragment two confirmed people both match
/// — two babies — is left untouched, never guessed), and touches only *unclaimed*
/// fragments (anything already bound to another identity is left alone). Runs only on
/// a settled library, where anchors are complete. Returns how many clusters folded in.
///
/// This is what turns "merge dozens of 1-photo clusters by hand" into "already done":
/// naming a person, or the sweep settling, reunites their scattered fragments with no
/// clicks. The manual review path remains only for the genuinely ambiguous residual.
/// For every candidate cluster, how well it matches *each confirmed identity* (named or
/// not), best-first — the shared basis for auto-fold and review. Because unnamed
/// "someone else" splits are confirmed identities too, they compete here: a face that
/// looks like a rejected look-alike is pulled toward that competitor and away from the
/// person, which is how a "not Mía" generalizes to similar faces.
pub fn cluster_identity_matches(
    conn: &Connection,
) -> anyhow::Result<std::collections::HashMap<i64, Vec<(i64, f32)>>> {
    let all_faces = db::face_cluster_embeddings(conn)?;
    cluster_identity_matches_with(conn, cluster::FaceMatrix::new(&all_faces).as_ref())
}

/// The shared-matrix form of [`cluster_identity_matches`]: the growth pass packs
/// the face set once and reuses it here, instead of paying a second full pack.
/// `None` (an empty library) yields an empty map.
fn cluster_identity_matches_with(
    conn: &Connection,
    face_matrix: Option<&cluster::FaceMatrix>,
) -> anyhow::Result<std::collections::HashMap<i64, Vec<(i64, f32)>>> {
    use std::collections::HashMap;
    let mut per_cluster: HashMap<i64, Vec<(i64, f32)>> = HashMap::new();
    let Some(fm) = face_matrix else { return Ok(per_cluster) };
    for identity_id in db::confirmed_identity_ids(conn)? {
        let anchor = db::confirmed_anchor_embeddings(conn, identity_id, 64)?;
        if anchor.len() < COMPETITOR_MIN {
            continue; // no confirmed evidence to compete with
        }
        let core = anchor_core(anchor);
        for c in fm.identity_candidates(&core, Some(-identity_id)) {
            per_cluster.entry(c.cluster_id).or_default().push((identity_id, c.max_sim));
        }
    }
    for v in per_cluster.values_mut() {
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    }
    Ok(per_cluster)
}

pub fn auto_fold_confident(conn: &Connection) -> anyhow::Result<usize> {
    use std::collections::HashSet;
    // Mid-sweep anchors are incomplete and would misfire — wait until scanning settles.
    match db::face_progress(conn) {
        Ok((scanned, eligible)) if eligible > 0 && scanned >= eligible => {}
        _ => return Ok(0),
    }
    if db::confirmed_identity_ids(conn)?.is_empty() {
        return Ok(0);
    }
    // Wipe the machine's previous tentative labels and re-derive them from scratch —
    // competitively — against the *confirmed* exemplars. This is what makes a wrong fold
    // self-correcting: nothing auto is welded on, so every pass reconsiders it against
    // whatever people (and "not X" competitors) you've since confirmed. Because a fold
    // writes only `identity_id` — never `cluster_id` — this whole pass is a cheap
    // re-derive: no clusters have to be un-merged, no re-cluster has to run.
    db::clear_unconfirmed_identities(conn)?;

    // Only identities with enough confirmed evidence may *claim* a cluster; a thin
    // competitor can still win the ranking (and thereby push a face off someone else)
    // but can't silently absorb it — that face just stays unassigned for review.
    let fold_eligible: HashSet<i64> =
        db::fold_eligible_identities(conn, MIN_ANCHOR as i64)?.into_iter().collect();
    let matches = cluster_identity_matches(conn)?;
    // Co-occurrence veto: a group photographed alongside the person's confirmed
    // faces cannot be them (siblings in one frame), however similar the embeddings.
    let (cluster_photos, identity_photos) = cooccurrence_maps(conn)?;

    // Assign each candidate to the identity it matches *decisively* best: the top match
    // must clear AUTO_FOLD_MIN and beat the runner-up by AUTO_FOLD_MARGIN. A near-tie
    // (two babies both plausible) is ambiguous — left unassigned for the review path,
    // never guessed.
    let mut folded = 0usize;
    for (cid, m) in matches {
        if cid < 0 {
            continue; // a person / competitor group — never folded away automatically
        }
        let (best_id, best_sim) = m[0];
        if best_sim < AUTO_FOLD_MIN {
            continue;
        }
        if m.len() > 1 && best_sim - m[1].1 < AUTO_FOLD_MARGIN {
            continue; // ambiguous between people — hold for review
        }
        if !fold_eligible.contains(&best_id) {
            continue; // best match is only a thin competitor — don't let it absorb
        }
        if cooccurs(&cluster_photos, cid, &identity_photos, best_id) {
            continue; // photographed together — two people, never fold
        }
        if db::assign_cluster_to_identity(conn, cid, best_id)? > 0 {
            folded += 1;
        }
    }
    Ok(folded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        db::init(&conn).unwrap();
        conn
    }

    /// A settled, face-scanned photo (the engines refuse to run mid-sweep).
    fn add_photo(conn: &Connection, id: i64) {
        conn.execute(
            "INSERT INTO photos (id, path, mtime, size, cache_key, thumb_status, seen, faces_scanned)
             VALUES (?1, ?2, 0, 0, '', 1, 0, 1)",
            rusqlite::params![id, format!("/p/{id}.jpg")],
        )
        .unwrap();
    }

    fn add_face(
        conn: &Connection,
        id: i64,
        photo: i64,
        cluster: i64,
        identity: Option<i64>,
        confirmed: bool,
        emb: &[f32],
    ) {
        let blob: Vec<u8> = emb.iter().flat_map(|v| v.to_le_bytes()).collect();
        conn.execute(
            "INSERT INTO faces (id, photo_id, x1, y1, x2, y2, score, embedding, cluster_id, identity_id, confirmed)
             VALUES (?1, ?2, 0.1, 0.1, 0.2, 0.2, 0.9, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, photo, blob, cluster, identity, confirmed as i64],
        )
        .unwrap();
    }

    /// A unit vector on `axis` with a small per-`k` wobble — one synthetic person.
    /// Wobble lands on dims axis+1..axis+6, so axes ≥ 8 apart never bleed together.
    fn near(axis: usize, k: usize) -> Vec<f32> {
        let dim = 16;
        let mut v = vec![0.0f32; dim];
        v[axis] = 1.0;
        v[(axis + 1 + k) % dim] += 0.03 * (k as f32 + 1.0);
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / n).collect()
    }

    fn cluster_of_face(conn: &Connection, face: i64) -> Option<i64> {
        conn.query_row("SELECT cluster_id FROM faces WHERE id = ?1", [face], |r| r.get(0))
            .unwrap()
    }

    fn identity_of_face(conn: &Connection, face: i64) -> Option<i64> {
        conn.query_row("SELECT identity_id FROM faces WHERE id = ?1", [face], |r| r.get(0))
            .unwrap()
    }

    /// Omar (4 confirmed exemplars) decisively matches a free look-alike cluster:
    /// auto-fold must absorb it — tentatively (confirmed stays 0, so the next pass
    /// can still re-decide it) and WITHOUT touching the appearance layer (nothing
    /// ever needs un-merging).
    #[test]
    fn auto_fold_absorbs_decisive_match() {
        let conn = test_conn();
        conn.execute("INSERT INTO identities (id, name) VALUES (1, 'Omar')", []).unwrap();
        for k in 0..4 {
            add_photo(&conn, 1 + k as i64);
            add_face(&conn, 1 + k as i64, 1 + k as i64, 1, Some(1), true, &near(0, k));
        }
        for k in 0..3 {
            add_photo(&conn, 11 + k as i64);
            add_face(&conn, 11 + k as i64, 11 + k as i64, 2, None, false, &near(0, k + 1));
        }
        let folded = auto_fold_confident(&conn).unwrap();
        assert_eq!(folded, 1, "the free cluster should fold into Omar");
        for f in 11..14 {
            assert_eq!(identity_of_face(&conn, f), Some(1), "displays under Omar");
            assert_eq!(cluster_of_face(&conn, f), Some(2), "appearance cluster untouched");
            let confirmed: i64 = conn
                .query_row("SELECT confirmed FROM faces WHERE id = ?1", [f], |r| r.get(0))
                .unwrap();
            assert_eq!(confirmed, 0, "auto-folds are tentative, never welded on");
        }
    }

    /// The self-heal promise, with no re-cluster anywhere: a cluster tentatively
    /// folded onto Omar is re-decided — and ejected to Kevin — the moment Kevin's
    /// confirmed anchor matches it decisively better. Appearance ids never move.
    #[test]
    fn refold_ejects_wrong_tentative_fold() {
        // A face part-way between Omar (axis 0) and a distinct axis-7 component:
        // ~0.65 to Omar (foldable when unopposed), ~1.0 to its own kind.
        fn mix(k: usize) -> Vec<f32> {
            let mut v = vec![0.0f32; 16];
            v[0] = 0.65;
            v[7] = 0.76;
            v[1 + k] += 0.02 * (k as f32 + 1.0);
            let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter().map(|x| x / n).collect()
        }
        let conn = test_conn();
        conn.execute("INSERT INTO identities (id, name) VALUES (1, 'Omar')", []).unwrap();
        for k in 0..4 {
            add_photo(&conn, 1 + k as i64);
            add_face(&conn, 1 + k as i64, 1 + k as i64, 1, Some(1), true, &near(0, k));
        }
        for k in 0..3 {
            add_photo(&conn, 11 + k as i64);
            add_face(&conn, 11 + k as i64, 11 + k as i64, 2, None, false, &mix(k));
        }
        auto_fold_confident(&conn).unwrap();
        assert_eq!(identity_of_face(&conn, 11), Some(1), "unopposed, the fold goes to Omar");

        // Kevin arrives: 4 confirmed faces of the candidate's own kind.
        conn.execute("INSERT INTO identities (id, name) VALUES (2, 'Kevin')", []).unwrap();
        for k in 0..4 {
            add_photo(&conn, 21 + k as i64);
            add_face(&conn, 21 + k as i64, 21 + k as i64, 3, Some(2), true, &mix(k));
        }
        auto_fold_confident(&conn).unwrap();
        for f in 11..14 {
            assert_eq!(identity_of_face(&conn, f), Some(2), "re-decided: ejected to Kevin");
            assert_eq!(cluster_of_face(&conn, f), Some(2), "appearance cluster untouched");
        }
    }

    /// The display invariant behind retiring most of the generation machinery: a
    /// full re-cluster may renumber every appearance id, but a person's group is
    /// keyed by their durable identity and survives verbatim.
    #[test]
    fn identity_groups_survive_appearance_renumbering() {
        let mut conn = test_conn();
        conn.execute("INSERT INTO identities (id, name) VALUES (1, 'Omar')", []).unwrap();
        for k in 0..3 {
            add_photo(&conn, 1 + k as i64);
            add_face(&conn, 1 + k as i64, 1 + k as i64, 5, Some(1), true, &near(0, k));
        }
        db::set_face_clusters(&mut conn, &[(1, 99), (2, 99), (3, 42)]).unwrap();
        let rows = db::clusters_overview(&conn).unwrap();
        let omar = rows.iter().find(|r| r.cluster_id == -1).expect("Omar's stable group");
        assert_eq!((omar.count, omar.name.as_deref()), (3, Some("Omar")));
    }

    /// Two confirmed people with (synthetically) identical anchors both match the
    /// candidate — a near-tie. Auto-fold must hold it for review, never guess.
    #[test]
    fn auto_fold_near_tie_is_left_for_review() {
        let conn = test_conn();
        conn.execute("INSERT INTO identities (id, name) VALUES (1, 'Omar'), (2, 'Kevin')", [])
            .unwrap();
        for k in 0..4 {
            add_photo(&conn, 1 + k as i64);
            add_face(&conn, 1 + k as i64, 1 + k as i64, 1, Some(1), true, &near(0, k));
            add_photo(&conn, 21 + k as i64);
            add_face(&conn, 21 + k as i64, 21 + k as i64, 2, Some(2), true, &near(0, k));
        }
        add_photo(&conn, 31);
        add_face(&conn, 31, 31, 3, None, false, &near(0, 5));
        let folded = auto_fold_confident(&conn).unwrap();
        assert_eq!(folded, 0, "a near-tie between two people must not be guessed");
        assert_eq!(cluster_of_face(&conn, 31), Some(3));
        assert_eq!(identity_of_face(&conn, 31), None);
    }

    /// A one-face identity (a fresh "not X" competitor) may compete but must not
    /// absorb: below MIN_ANCHOR it lacks the evidence to claim a whole cluster.
    #[test]
    fn auto_fold_thin_competitor_cannot_absorb() {
        let conn = test_conn();
        conn.execute("INSERT INTO identities (id, name) VALUES (1, NULL)", []).unwrap();
        add_photo(&conn, 1);
        add_face(&conn, 1, 1, 1, Some(1), true, &near(0, 0));
        for k in 0..3 {
            add_photo(&conn, 11 + k as i64);
            add_face(&conn, 11 + k as i64, 11 + k as i64, 2, None, false, &near(0, k + 1));
        }
        let folded = auto_fold_confident(&conn).unwrap();
        assert_eq!(folded, 0, "a thin competitor must not vacuum up a cluster");
        assert_eq!(identity_of_face(&conn, 11), None);
        assert_eq!(cluster_of_face(&conn, 11), Some(2));
    }

    /// A candidate photographed alongside the person's confirmed faces cannot BE
    /// the person (two faces in one photo are two people), however similar.
    #[test]
    fn auto_fold_cooccurrence_veto() {
        let conn = test_conn();
        conn.execute("INSERT INTO identities (id, name) VALUES (1, 'Omar')", []).unwrap();
        for k in 0..4 {
            add_photo(&conn, 1 + k as i64);
            add_face(&conn, 1 + k as i64, 1 + k as i64, 1, Some(1), true, &near(0, k));
        }
        // Candidate shares photo 4 with a confirmed Omar face.
        add_face(&conn, 11, 4, 2, None, false, &near(0, 5));
        add_photo(&conn, 12);
        add_face(&conn, 12, 12, 2, None, false, &near(0, 6));
        let folded = auto_fold_confident(&conn).unwrap();
        assert_eq!(folded, 0, "a co-occurring cluster must never fold in");
        assert_eq!(identity_of_face(&conn, 11), None);
        assert_eq!(cluster_of_face(&conn, 11), Some(2));
    }

    /// A cluster claimed by two named people is dropped from BOTH growth cards and
    /// surfaces as a who-is-this review card instead — the near-ties auto-fold
    /// refuses to guess become the queue's best question, never a silent merge.
    #[test]
    fn growth_conflict_becomes_who_is_this() {
        let conn = test_conn();
        conn.execute("INSERT INTO identities (id, name) VALUES (1, 'Omar'), (2, 'Kevin')", [])
            .unwrap();
        for k in 0..4 {
            add_photo(&conn, 1 + k as i64);
            add_face(&conn, 1 + k as i64, 1 + k as i64, 1, Some(1), true, &near(0, k));
            add_photo(&conn, 21 + k as i64);
            add_face(&conn, 21 + k as i64, 21 + k as i64, 2, Some(2), true, &near(0, k));
        }
        for k in 0..3 {
            add_photo(&conn, 31 + k as i64);
            add_face(&conn, 31 + k as i64, 31 + k as i64, 3, None, false, &near(0, k + 4));
        }
        let (growth, who) = compute_identity_growth(&conn).unwrap();
        for g in &growth {
            assert!(
                !g.strong_clusters.contains(&3) && g.maybe.iter().all(|m| m.cluster_id != 3),
                "a contested cluster must not appear on {}'s growth card",
                g.name
            );
        }
        assert_eq!(who.len(), 1, "the contested cluster becomes one who-is-this card");
        match &who[0] {
            ReviewItem::WhoIsThis { cluster_id, candidates, .. } => {
                assert_eq!(*cluster_id, 3);
                assert_eq!(candidates.len(), 2);
            }
            other => panic!("expected WhoIsThis, got {:?}", std::mem::discriminant(other)),
        }
    }

    /// The queue is payoff-sorted (photos desc) across every item kind.
    #[test]
    fn review_queue_sorts_by_payoff() {
        let merges = vec![MergeSuggestion {
            into: 1,
            from: 2,
            into_faces: vec![],
            from_faces: vec![],
            into_name: None,
            similarity: 0.8,
            photos: 5,
            generation: 0,
        }];
        let growth = vec![IdentityGrowth {
            identity_id: 1,
            name: "Omar".into(),
            into: 1,
            anchor_faces: vec![],
            strong_clusters: vec![9],
            strong_groups: vec![GrowthCluster {
                cluster_id: 9,
                face_id: None,
                photos: 50,
                similarity: 0.9,
            }],
            strong_faces: vec![],
            strong_photos: 50,
            maybe: vec![GrowthCluster {
                cluster_id: 8,
                face_id: None,
                photos: 20,
                similarity: 0.55,
            }],
            photos: 70,
            generation: 0,
        }];
        let queue = build_review_queue(&merges, &growth, Vec::new());
        let photos: Vec<i64> = queue
            .iter()
            .map(|i| match i {
                ReviewItem::StrongBatch { photos, .. }
                | ReviewItem::Maybe { photos, .. }
                | ReviewItem::WhoIsThis { photos, .. }
                | ReviewItem::Pairwise { photos, .. }
                | ReviewItem::SamePhotoTwin { photos, .. } => *photos,
            })
            .collect();
        assert_eq!(photos, vec![50, 20, 5], "biggest payoff first");
    }

    /// Cards below the payoff floor stay out of the focus session — a 1-photo
    /// question is a chore there; it lives on the person page / People instead.
    #[test]
    fn review_queue_drops_low_payoff_cards() {
        let merges = vec![
            MergeSuggestion {
                into: 1,
                from: 2,
                into_faces: vec![],
                from_faces: vec![],
                into_name: None,
                similarity: 0.8,
                photos: REVIEW_MIN_PHOTOS, // exactly at the floor: stays
                generation: 0,
            },
            MergeSuggestion {
                into: 3,
                from: 4,
                into_faces: vec![],
                from_faces: vec![],
                into_name: None,
                similarity: 0.9,
                photos: 1, // a singleton: filtered
                generation: 0,
            },
        ];
        let queue = build_review_queue(&merges, &[], Vec::new());
        assert_eq!(queue.len(), 1, "only the at-floor card survives");
        assert!(
            matches!(&queue[0], ReviewItem::Pairwise { photos, .. } if *photos == REVIEW_MIN_PHOTOS)
        );
    }

    /// The anchor core drops a minority outlier (a wrong fold / off-angle shot) so
    /// it can't drag the anchor off the person.
    #[test]
    fn anchor_core_drops_outlier() {
        let mut embs: Vec<Vec<f32>> = (0..5).map(|k| near(0, k)).collect();
        embs.push(near(8, 0)); // the pollution
        let core = anchor_core(embs);
        assert_eq!(core.len(), 5, "the dominant appearance group is the core");
        assert!(core.iter().all(|e| e[0] > 0.9), "the outlier must be dropped");
    }

}
