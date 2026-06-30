//! Clustering of face embeddings into people — purity first.
//!
//! Governing law (PRINCIPLES.md #5): **optimize for cluster purity, not
//! completeness.** Merging two pure piles is one batch click; un-mixing a polluted
//! pile is face-by-face misery. So we bias hard toward *not* merging: a person
//! split across a few clean clusters is fine, a cluster holding two people is not.
//!
//! The old approach matched each face to the nearest running-mean *centroid* and
//! updated the mean. A few unlucky early assignments polluted a mean, and the
//! polluted mean became a magnet that pulled in ever more different faces — a
//! "pollution snowball" that produced one cluster spanning 20 years and four
//! people. There is no centroid here. Evidence is **face-to-face** only.
//!
//! ## Batch [`recluster`] (order-independent)
//! 1. **kNN graph.** Each face's `K` nearest other faces by cosine similarity
//!    (embeddings are L2-normalized, so cosine == dot product). Computed with
//!    blocked matrix multiplication so memory stays bounded at ~100k faces.
//! 2. **Mutual edges only.** Keep an edge `(i, j)` only if each is in the other's
//!    top-`K` and the similarity clears [`TAU_LINK`]. Mutuality kills "hub" faces
//!    (a generic-looking face near everyone) that would otherwise chain the world
//!    into one blob.
//! 3. **Complete-linkage agglomeration (the anti-chaining guard).** Walk the
//!    candidate edges strongest-first, merging the two faces' groups only if
//!    *every* cross-pair between them clears [`TAU_LINK`] — complete linkage. The
//!    result is that each cluster is a clique in the τ-graph: no two faces inside
//!    it are below threshold. A bridge face may join the one group it's genuinely
//!    close to, but it can never fuse two groups, because the far group's members
//!    fail the all-pairs test. This is the concrete embodiment of "over-merge =
//!    catastrophe," and it makes the result order-independent (it depends only on
//!    the edge similarities, not on face arrival order).
//! 4. A face with no qualifying edge is its own cluster — over-split is the safe
//!    state, collapsed later by a single trustworthy merge click.
//!
//! ## Incremental [`ClusterIndex::assign`] (new faces after the batch)
//! No running mean. A new face votes among its nearest already-clustered faces: it
//! joins the majority cluster among neighbors above [`TAU_LINK`] (needing real
//! agreement, not a single weak link) — but only if it also clears [`TAU_LINK`]
//! against *every* member of that cluster, the same complete-linkage purity test the
//! batch path uses. That keeps each cluster a clique even mid-scan, so a
//! mid-similarity "bridge" face cannot vote its way into a pile and chain two people
//! together before the next consolidation. A lone near-duplicate neighbor
//! ([`TAU_DUP`]) is still decisive on its own. One outlier face can never pollute a
//! cluster because there is no mean to drag. The full batch [`recluster`] is re-run
//! periodically to consolidate.

use ndarray::Array2;

/// Neighbors examined per face when building the graph / voting.
const K: usize = 10;
/// Minimum cosine similarity for two faces to be considered linkable. Set
/// conservatively above SFace's ~0.363 balance point so we err toward purity;
/// raise toward 0.55 if any mixed cluster survives (tune from the histogram).
const TAU_LINK: f32 = 0.5;
/// Similarity at which two faces are treated as near-duplicates — strong enough
/// to link a true pair on its own, without needing a corroborating triangle.
const TAU_DUP: f32 = 0.8;
/// Incremental: how many neighbors must agree on a cluster to join it (below this
/// only a near-duplicate, single strong neighbor, can pull a face in).
const MIN_VOTE: usize = 2;
/// Merge suggestions: the lowest similarity a cross-cluster face pair may have and
/// still count as evidence the two clusters are one person. Deliberately below
/// [`TAU_LINK`] — a suggestion is a *prompt*, confirmed by the user, so it can reach
/// a little further than automatic clustering dares.
const TAU_SUGGEST: f32 = 0.45;
/// Merge suggestions: how many qualifying cross-cluster face pairs two clusters
/// need before we'll suggest merging them. Several pairs (not one centroid angle)
/// is what makes a suggestion trustworthy — the fix for "you vs. grandma".
const MIN_PAIRS: usize = 3;

fn normalized(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    v.iter().map(|x| x / n).collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Bounded top-`K` selection: insert `(idx, sim)` keeping `top` sorted descending
/// and capped at `K`. Cheap because `K` is tiny.
fn push_topk(top: &mut Vec<(usize, f32)>, idx: usize, sim: f32) {
    if top.len() < K {
        top.push((idx, sim));
        top.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    } else if sim > top[K - 1].1 {
        top[K - 1] = (idx, sim);
        top.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    }
}

/// Disjoint-set (union-find) over face indices, for connected components.
struct UnionFind {
    parent: Vec<usize>,
}
impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind { parent: (0..n).collect() }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
}

/// Build the per-face top-`K` neighbor lists (index + similarity) over the whole
/// set, using blocked matrix multiplication to bound transient memory. `progress`
/// is called with a fraction in `[0, 1)` as blocks complete.
fn knn_graph<F: FnMut(f32)>(mat: &Array2<f32>, min_sim: f32, mut progress: F) -> Vec<Vec<(usize, f32)>> {
    let n = mat.nrows();
    let mut neighbors: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
    // ~B*N floats transient per block; B=256 keeps that near 100 MB at 100k faces.
    const BLOCK: usize = 256;
    let mut start = 0;
    while start < n {
        let end = (start + BLOCK).min(n);
        // (B x dim) . (dim x N) -> (B x N) similarities for this block of rows.
        let sims = mat.slice(ndarray::s![start..end, ..]).dot(&mat.t());
        for (bi, row) in sims.outer_iter().enumerate() {
            let i = start + bi;
            let top = &mut neighbors[i];
            for (j, &s) in row.iter().enumerate() {
                if j != i && s >= min_sim {
                    push_topk(top, j, s);
                }
            }
        }
        start = end;
        progress(start as f32 / n as f32);
    }
    neighbors
}

/// Build the (re-normalized) N×dim embedding matrix from `(.., embedding)` rows.
fn embedding_matrix<T>(faces: &[(T, Vec<f32>)]) -> Array2<f32> {
    let n = faces.len();
    let dim = faces[0].1.len();
    let mut data = Vec::with_capacity(n * dim);
    for (_, e) in faces {
        data.extend(normalized(e));
    }
    Array2::from_shape_vec((n, dim), data).expect("uniform embedding length")
}

/// Re-cluster every face from scratch — order-independent, purity-biased.
///
/// Input is `(face_id, embedding)`; output maps every input `face_id` to a new
/// 1-based `cluster_id`. Pure (no DB, no globals) so it is unit-tested directly.
/// `progress` reports a fraction in `[0, 1]` for the long kNN phase.
pub fn recluster<F: FnMut(f32)>(faces: &[(i64, Vec<f32>)], mut progress: F) -> Vec<(i64, i64)> {
    let n = faces.len();
    if n == 0 {
        return Vec::new();
    }
    // Defensive re-normalization (inside `embedding_matrix`) so cosine == dot even
    // if a stored vector drifted.
    let mat = embedding_matrix(faces);

    let neighbors = knn_graph(&mat, TAU_LINK, |f| progress(f * 0.5));

    // Candidate edges: every kNN pair (either direction), deduped to i<j, carrying
    // its similarity. These are only *candidates* — the complete-linkage test below
    // decides which actually merge. Strongest first so tight groups form before any
    // looser edge is considered.
    use std::collections::HashSet;
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    let mut edges: Vec<(f32, usize, usize)> = Vec::new();
    for (i, nbrs) in neighbors.iter().enumerate() {
        for &(j, sim) in nbrs {
            let (a, b) = if i < j { (i, j) } else { (j, i) };
            if seen.insert((a, b)) {
                edges.push((sim, a, b));
            }
        }
    }
    edges.sort_unstable_by(|x, y| y.0.partial_cmp(&x.0).unwrap());

    // Greedy complete-linkage agglomeration. `members[root]` is the face set of the
    // cluster rooted at `root`; two clusters merge only if all cross-pairs clear
    // TAU_LINK, keeping every cluster a clique (pure by construction).
    let mut uf = UnionFind::new(n);
    let mut members: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
    let total = edges.len().max(1);
    for (idx, &(_, a, b)) in edges.iter().enumerate() {
        let (ra, rb) = (uf.find(a), uf.find(b));
        if ra != rb && complete_link(&mat, &members[ra], &members[rb]) {
            // Fold the smaller set into the larger so root membership stays cheap.
            let (keep, gone) = if members[ra].len() >= members[rb].len() { (ra, rb) } else { (rb, ra) };
            let moved = std::mem::take(&mut members[gone]);
            members[keep].extend(moved);
            uf.parent[gone] = keep;
        }
        if idx % 4096 == 0 {
            progress(0.5 + 0.5 * idx as f32 / total as f32);
        }
    }
    progress(1.0);

    // Map each component root to a compact 1-based cluster id.
    use std::collections::HashMap;
    let mut root_to_cid: HashMap<usize, i64> = HashMap::new();
    let mut next: i64 = 1;
    let mut out = Vec::with_capacity(n);
    for (i, (face_id, _)) in faces.iter().enumerate() {
        let root = uf.find(i);
        let cid = *root_to_cid.entry(root).or_insert_with(|| {
            let c = next;
            next += 1;
            c
        });
        out.push((*face_id, cid));
    }
    out
}

/// Complete-linkage test: true iff *every* cross-pair between the two member sets
/// has cosine ≥ [`TAU_LINK`]. Early-exits on the first pair that fails, so an
/// obvious non-merge is cheap. Cosine == dot since rows are normalized.
fn complete_link(mat: &Array2<f32>, a: &[usize], b: &[usize]) -> bool {
    for &i in a {
        let ri = mat.row(i);
        for &j in b {
            if ri.dot(&mat.row(j)) < TAU_LINK {
                return false;
            }
        }
    }
    true
}

/// Cosine similarities of every mutual-kNN edge over the face set. This is the
/// distribution that should set [`TAU_LINK`] from a real library rather than from
/// vibes: a clean separation shows up as a gap between the within-person mass and
/// the across-person tail. Exposed for the `cluster_debug` command.
pub fn mutual_edge_sims(faces: &[(i64, Vec<f32>)]) -> Vec<f32> {
    let n = faces.len();
    if n == 0 {
        return Vec::new();
    }
    let mat = embedding_matrix(faces);
    let neighbors = knn_graph(&mat, TAU_LINK, |_| {});
    let nbr_sets: Vec<std::collections::HashSet<usize>> = neighbors
        .iter()
        .map(|v| v.iter().map(|&(j, _)| j).collect())
        .collect();
    let mut sims = Vec::new();
    for i in 0..n {
        for &(j, sim) in &neighbors[i] {
            if i < j && nbr_sets[j].contains(&i) {
                sims.push(sim);
            }
        }
    }
    sims
}

/// Evidence that two clusters are the same person: the count of qualifying
/// cross-cluster face pairs and the strongest such pair's similarity.
pub struct PairEvidence {
    pub a: i64,
    pub b: i64,
    pub pairs: usize,
    pub max_sim: f32,
}

/// Face-to-face merge evidence between clusters — the trustworthy replacement for
/// centroid-to-centroid suggestions (which surfaced "you vs. grandma"). Input is
/// `(face_id, cluster_id, embedding)`. For every pair of distinct-cluster faces
/// that are near neighbors (cosine ≥ [`TAU_SUGGEST`]) we tally the cluster pair;
/// only cluster pairs backed by at least [`MIN_PAIRS`] such face pairs are
/// returned. Several independent face matches — not one lucky angle — is what
/// makes a suggestion one you say yes to.
pub fn merge_evidence(faces: &[(i64, i64, Vec<f32>)]) -> Vec<PairEvidence> {
    let n = faces.len();
    if n == 0 {
        return Vec::new();
    }
    // Reuse the matrix/kNN machinery, dropping the cluster id for the embedding.
    let rows: Vec<(i64, Vec<f32>)> = faces.iter().map(|(f, _, e)| (*f, e.clone())).collect();
    let mat = embedding_matrix(&rows);
    let neighbors = knn_graph(&mat, TAU_SUGGEST, |_| {});
    let cluster_of: Vec<i64> = faces.iter().map(|(_, c, _)| *c).collect();

    use std::collections::HashMap;
    // (min_cluster, max_cluster) -> (pair count, max similarity).
    let mut tally: HashMap<(i64, i64), (usize, f32)> = HashMap::new();
    for i in 0..n {
        for &(j, sim) in &neighbors[i] {
            if i >= j {
                continue; // each unordered face pair once
            }
            let (ca, cb) = (cluster_of[i], cluster_of[j]);
            if ca == cb {
                continue; // same cluster — nothing to suggest
            }
            let key = if ca < cb { (ca, cb) } else { (cb, ca) };
            let e = tally.entry(key).or_insert((0, 0.0));
            e.0 += 1;
            e.1 = e.1.max(sim);
        }
    }
    tally
        .into_iter()
        .filter(|&(_, (pairs, _))| pairs >= MIN_PAIRS)
        .map(|((a, b), (pairs, max_sim))| PairEvidence { a, b, pairs, max_sim })
        .collect()
}

/// A cluster the magnet judges to be the same person as a confirmed identity:
/// how many of its faces match the anchor, the cluster's size, and the strongest
/// match. `matched == size` means the whole group is confidently that person.
pub struct GrowthCandidate {
    pub cluster_id: i64,
    pub size: usize,
    pub max_sim: f32,
}

/// Anchored confidence propagation — the engine behind "you confirmed Omar, here
/// are 40 more groups that are also Omar." Given a confirmed identity's `anchor`
/// face profile and every *other* clustered face, return the clusters whose faces
/// confidently match the anchor.
///
/// This is safe to reach where unsupervised clustering dares not, for two reasons:
/// the anchor is **human-confirmed** (not a drifting centroid), and matching is to
/// the anchor only — never chained cluster→cluster — so it can't fuse strangers the
/// way transitive merging would. With these embeddings, different people sit far
/// below [`TAU_LINK`], so a cluster whose faces clear it against Omar's profile is
/// Omar. We require a *majority* of the cluster to match, so a single stray face
/// can't drag a mixed group in.
pub fn identity_candidates(
    anchor: &[Vec<f32>],
    others: &[(i64, i64, Vec<f32>)],
) -> Vec<GrowthCandidate> {
    if anchor.is_empty() || others.is_empty() {
        return Vec::new();
    }
    let dim = anchor[0].len();
    let a = Array2::from_shape_vec(
        (anchor.len(), dim),
        anchor.iter().flat_map(|e| normalized(e)).collect(),
    )
    .expect("uniform anchor length");
    let o = Array2::from_shape_vec(
        (others.len(), dim),
        others.iter().flat_map(|(_, _, e)| normalized(e)).collect(),
    )
    .expect("uniform other length");
    // best = each other face's strongest cosine to any anchor face.
    let sims = o.dot(&a.t()); // (others x anchor)
    use std::collections::HashMap;
    // cluster_id -> (size, matched, max_sim)
    let mut tally: HashMap<i64, (usize, usize, f32)> = HashMap::new();
    for (i, (_, cid, _)) in others.iter().enumerate() {
        let best = sims.row(i).iter().cloned().fold(f32::MIN, f32::max);
        let e = tally.entry(*cid).or_insert((0, 0, 0.0));
        e.0 += 1;
        if best >= TAU_LINK {
            e.1 += 1;
            e.2 = e.2.max(best);
        }
    }
    tally
        .into_iter()
        // A confident candidate: most of the cluster matches the anchor, with at
        // least a couple of corroborating faces (or the whole of a tiny cluster).
        .filter(|&(_, (size, matched, _))| matched * 2 >= size && matched >= 2.min(size).max(1))
        .map(|(cluster_id, (size, _matched, max_sim))| GrowthCandidate {
            cluster_id,
            size,
            max_sim,
        })
        .collect()
}

/// In-memory state for incremental assignment of newly-detected faces. Holds every
/// already-clustered face's `(cluster_id, embedding)` so a new face can vote among
/// its nearest neighbors. Rebuilt from the DB at startup, so a restart reproduces
/// the same state.
pub struct ClusterIndex {
    faces: Vec<(i64, Vec<f32>)>, // (cluster_id, embedding)
    next_id: i64,
}

impl ClusterIndex {
    /// Rebuild from `(cluster_id, embedding)` rows of the already-clustered faces.
    pub fn load(rows: Vec<(i64, Vec<f32>)>) -> Self {
        let next_id = rows.iter().map(|(c, _)| *c).max().unwrap_or(0) + 1;
        ClusterIndex { faces: rows, next_id }
    }

    /// Assign a new embedding by majority vote among its nearest clustered faces —
    /// no centroid, so no single face can drag a cluster. Returns the cluster id
    /// (an existing one, or a fresh singleton). The face is remembered so later
    /// faces in the same batch can match against it.
    pub fn assign(&mut self, emb: &[f32]) -> i64 {
        use std::collections::HashMap;
        let q = normalized(emb);
        // One pass over every clustered face: collect the Top-K nearest above the
        // link threshold (the vote candidates) and, separately, each cluster's
        // *minimum* similarity across *all* its members — including those below
        // TAU_LINK, which never enter `top`. That minimum is the complete-linkage
        // guard: a cluster is a valid join target only if the new face clears
        // TAU_LINK against every one of its members, which keeps the cluster a clique
        // and stops a mid-similarity bridge face from accreting two people into one
        // pile during the live scan.
        let mut top: Vec<(usize, f32)> = Vec::new();
        let mut cluster_min: HashMap<i64, f32> = HashMap::new();
        for (idx, (cid, e)) in self.faces.iter().enumerate() {
            let s = cosine(&q, e);
            let m = cluster_min.entry(*cid).or_insert(f32::INFINITY);
            *m = m.min(s);
            if s >= TAU_LINK {
                push_topk(&mut top, idx, s);
            }
        }

        let chosen = self.vote(&top, &cluster_min);
        let cid = chosen.unwrap_or_else(|| {
            let c = self.next_id;
            self.next_id += 1;
            c
        });
        self.faces.push((cid, q));
        cid
    }

    /// Pick a cluster from the neighbor list: the plurality cluster, but only if it
    /// has real support (`MIN_VOTE` neighbors) *and* the new face clears [`TAU_LINK`]
    /// against every member of it (`cluster_min` — complete linkage, so the join
    /// keeps the cluster a clique). A single near-duplicate neighbor is still enough
    /// on its own: a ≥[`TAU_DUP`] match is unambiguous and is never the bridge that
    /// chains two people, so it bypasses the all-members test (and so a confirmed
    /// identity, whose members can be spread, keeps accreting its duplicates).
    /// `None` ⇒ singleton.
    fn vote(&self, top: &[(usize, f32)], cluster_min: &std::collections::HashMap<i64, f32>) -> Option<i64> {
        if top.is_empty() {
            return None;
        }
        if top[0].1 >= TAU_DUP {
            return Some(self.faces[top[0].0].0);
        }
        use std::collections::HashMap;
        let mut counts: HashMap<i64, usize> = HashMap::new();
        for &(idx, _) in top {
            *counts.entry(self.faces[idx].0).or_insert(0) += 1;
        }
        counts
            .into_iter()
            .filter(|&(cid, c)| c >= MIN_VOTE && cluster_min.get(&cid).map_or(false, |&m| m >= TAU_LINK))
            .max_by_key(|&(_, c)| c)
            .map(|(cid, _)| cid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit vector pointing mostly along axis `axis` with a little spread, so a
    /// "blob" is a set of near-but-not-identical faces of one synthetic person.
    fn vec_near(dim: usize, axis: usize, jitter: usize, scale: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; dim];
        v[axis] = 1.0;
        v[(jitter + 1) % dim] += scale; // perturb a different component each time
        normalized(&v)
    }

    fn blob(dim: usize, axis: usize, count: usize) -> Vec<Vec<f32>> {
        (0..count).map(|k| vec_near(dim, axis, axis * 10 + k, 0.05 * k as f32)).collect()
    }

    fn cluster_of(assignments: &[(i64, i64)], face_id: i64) -> i64 {
        assignments.iter().find(|(f, _)| *f == face_id).unwrap().1
    }

    #[test]
    fn well_separated_blobs_do_not_merge() {
        // Two tight groups on orthogonal axes — cross-similarity ~0, far below TAU.
        let mut faces = Vec::new();
        for (k, e) in blob(16, 0, 5).into_iter().enumerate() {
            faces.push((k as i64, e));
        }
        for (k, e) in blob(16, 8, 5).into_iter().enumerate() {
            faces.push((100 + k as i64, e));
        }
        let a = recluster(&faces, |_| {});
        // All of group A share one cluster; all of group B share another; distinct.
        let ca = cluster_of(&a, 0);
        let cb = cluster_of(&a, 100);
        assert_ne!(ca, cb, "orthogonal blobs must not merge");
        for k in 0..5 {
            assert_eq!(cluster_of(&a, k), ca);
            assert_eq!(cluster_of(&a, 100 + k), cb);
        }
    }

    #[test]
    fn single_bridge_does_not_chain_two_blobs() {
        // Two blobs plus one face sitting partway between them, weakly linked to a
        // single member of each. The mutual+triangle guard must refuse to chain.
        let dim = 16;
        let mut faces = Vec::new();
        for (k, e) in blob(dim, 0, 5).into_iter().enumerate() {
            faces.push((k as i64, e));
        }
        for (k, e) in blob(dim, 8, 5).into_iter().enumerate() {
            faces.push((100 + k as i64, e));
        }
        // Bridge: equal mix of the two axes — similar to each blob but not a member.
        let mut bridge = vec![0.0f32; dim];
        bridge[0] = 0.72;
        bridge[8] = 0.72;
        faces.push((999, normalized(&bridge)));

        let a = recluster(&faces, |_| {});
        let ca = cluster_of(&a, 0);
        let cb = cluster_of(&a, 100);
        assert_ne!(ca, cb, "bridge must not chain the two blobs together");
    }

    #[test]
    fn near_duplicates_group() {
        // A pair of almost-identical faces (cosine well above TAU_DUP) must land
        // in the same cluster even without a third face to form a triangle.
        let dim = 16;
        let mut a = vec![0.0f32; dim];
        a[3] = 1.0;
        a[4] = 0.02;
        let mut b = vec![0.0f32; dim];
        b[3] = 1.0;
        b[4] = 0.01;
        let faces = vec![(1i64, normalized(&a)), (2i64, normalized(&b))];
        let asn = recluster(&faces, |_| {});
        assert_eq!(cluster_of(&asn, 1), cluster_of(&asn, 2));
    }

    #[test]
    fn incremental_outlier_starts_its_own_cluster() {
        // Seed a cluster, then assign a far-away face: it must NOT join (no magnet).
        let seed: Vec<(i64, Vec<f32>)> = blob(16, 0, 4).into_iter().map(|e| (1, e)).collect();
        let mut idx = ClusterIndex::load(seed);
        let mut outlier = vec![0.0f32; 16];
        outlier[8] = 1.0;
        let cid = idx.assign(&normalized(&outlier));
        assert_ne!(cid, 1, "an outlier must start a new cluster, not pollute cluster 1");
    }

    #[test]
    fn merge_evidence_finds_split_person_not_strangers() {
        // Clusters 1 and 2 are the same person split in two (both near axis 0, so
        // many cross-pairs above TAU_SUGGEST). Cluster 3 is a stranger on axis 8.
        let dim = 16;
        let mut faces: Vec<(i64, i64, Vec<f32>)> = Vec::new();
        for (k, e) in blob(dim, 0, 5).into_iter().enumerate() {
            faces.push((k as i64, 1, e));
        }
        for (k, e) in blob(dim, 0, 5).into_iter().enumerate() {
            faces.push((100 + k as i64, 2, e));
        }
        for (k, e) in blob(dim, 8, 5).into_iter().enumerate() {
            faces.push((200 + k as i64, 3, e));
        }
        let ev = merge_evidence(&faces);
        let has = |x: i64, y: i64| {
            ev.iter().any(|e| (e.a, e.b) == (x.min(y), x.max(y)) && e.pairs >= MIN_PAIRS)
        };
        assert!(has(1, 2), "the split person (1,2) should be suggested");
        assert!(!has(1, 3) && !has(2, 3), "strangers must not be suggested");
    }

    #[test]
    fn incremental_neighbor_joins_cluster() {
        let seed: Vec<(i64, Vec<f32>)> = blob(16, 0, 4).into_iter().map(|e| (7, e)).collect();
        let mut idx = ClusterIndex::load(seed);
        // A new face squarely inside the blob should join cluster 7.
        let cid = idx.assign(&vec_near(16, 0, 3, 0.04));
        assert_eq!(cid, 7);
    }

    #[test]
    fn incremental_complete_linkage_blocks_bridge() {
        // A cluster that has spread — two near members on axis 0 plus a far member
        // only ~0.45 to them — the shape single-linkage accretion produces. A new
        // face links to the two near members by the 2-vote rule (mid-similarity, not
        // a near-duplicate) but sits below TAU_LINK from the far member, so the
        // cluster is not a clique against it. The complete-linkage guard must refuse
        // the join — it starts its own cluster rather than grow a non-pure pile.
        let dim = 16;
        let mut m1 = vec![0.0f32; dim];
        m1[0] = 1.0;
        let mut m2 = vec![0.0f32; dim];
        m2[0] = 1.0;
        m2[1] = 0.05;
        // Far member: 0.45 cosine to the axis-0 pair (rest of its mass on axis 5).
        let mut m3 = vec![0.0f32; dim];
        m3[0] = 0.45;
        m3[5] = (1.0f32 - 0.45 * 0.45).sqrt();
        let seed = vec![(1i64, normalized(&m1)), (1, normalized(&m2)), (1, normalized(&m3))];
        let mut idx = ClusterIndex::load(seed);
        // Candidate: ~0.6 to m1/m2 (two votes, but below TAU_DUP so it must pass the
        // guard) and only ~0.27 to m3.
        let mut cand = vec![0.0f32; dim];
        cand[0] = 0.6;
        cand[7] = 0.8;
        let cid = idx.assign(&normalized(&cand));
        assert_ne!(cid, 1, "a bridge face must not join a cluster it fails complete-linkage against");
    }

    #[test]
    fn identity_magnet_pulls_same_person_not_strangers() {
        // Anchor = a confirmed person on axis 0. Cluster 2 is the same person (axis
        // 0) split off; cluster 3 is a stranger on axis 8. The magnet should offer
        // cluster 2 and never cluster 3.
        let dim = 16;
        let anchor: Vec<Vec<f32>> = blob(dim, 0, 5);
        let mut others: Vec<(i64, i64, Vec<f32>)> = Vec::new();
        for (k, e) in blob(dim, 0, 4).into_iter().enumerate() {
            others.push((k as i64, 2, e));
        }
        for (k, e) in blob(dim, 8, 4).into_iter().enumerate() {
            others.push((100 + k as i64, 3, e));
        }
        let cands = identity_candidates(&anchor, &others);
        assert!(cands.iter().any(|c| c.cluster_id == 2), "same person must be offered");
        assert!(!cands.iter().any(|c| c.cluster_id == 3), "a stranger must never be offered");
    }
}
