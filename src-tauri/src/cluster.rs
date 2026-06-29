//! Incremental online clustering of face embeddings into people.
//!
//! Each face is assigned to the nearest existing cluster centroid if their cosine
//! similarity clears a threshold, otherwise it starts a new cluster. This is
//! O(faces × clusters) — cheap, streaming, and resumable — which suits the
//! background worker and scales to large libraries (Principle 6).
//!
//! We bias toward *more* clusters (a conservative threshold): over-splitting a
//! person into two clusters is recoverable via the "same person?" merge UX,
//! whereas merging two different people is not. Clusters are identified by the
//! integer `cluster_id` stored on each face; centroids are derived from members,
//! so a restart rebuilds the exact same state from the database.
//!
//! Threshold tuned against real embeddings: at 0.5 two (similar-looking) people
//! collapsed into one cluster; 0.65 separated them into balanced clusters while
//! leaving only a few hard-angle singletons for the merge UX to absorb.

const ASSIGN_THR: f32 = 0.65;

struct Cluster {
    id: i64,
    sum: Vec<f32>,
    centroid: Vec<f32>,
}

pub struct ClusterIndex {
    clusters: Vec<Cluster>,
    next_id: i64,
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn normalized(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    v.iter().map(|x| x / n).collect()
}

impl ClusterIndex {
    /// Rebuild the in-memory centroids from the faces already clustered in the DB.
    pub fn load(rows: Vec<(i64, Vec<f32>)>) -> Self {
        let mut by_id: std::collections::HashMap<i64, Vec<f32>> = std::collections::HashMap::new();
        let mut max_id = 0i64;
        for (cid, emb) in rows {
            max_id = max_id.max(cid);
            let sum = by_id.entry(cid).or_insert_with(|| vec![0.0; emb.len()]);
            if sum.len() == emb.len() {
                for (s, e) in sum.iter_mut().zip(&emb) {
                    *s += e;
                }
            }
        }
        let clusters = by_id
            .into_iter()
            .map(|(id, sum)| {
                let centroid = normalized(&sum);
                Cluster { id, sum, centroid }
            })
            .collect();
        ClusterIndex { clusters, next_id: max_id + 1 }
    }

    /// Assign an embedding to a cluster, updating centroids. Returns the cluster id.
    pub fn assign(&mut self, emb: &[f32]) -> i64 {
        let mut best = -2.0f32;
        let mut best_i: Option<usize> = None;
        for (i, c) in self.clusters.iter().enumerate() {
            let s = cosine(emb, &c.centroid);
            if s > best {
                best = s;
                best_i = Some(i);
            }
        }
        if let Some(i) = best_i {
            if best >= ASSIGN_THR {
                let c = &mut self.clusters[i];
                for (s, e) in c.sum.iter_mut().zip(emb) {
                    *s += e;
                }
                c.centroid = normalized(&c.sum);
                return c.id;
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        self.clusters.push(Cluster {
            id,
            sum: emb.to_vec(),
            centroid: normalized(emb),
        });
        id
    }
}
