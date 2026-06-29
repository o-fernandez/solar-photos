// The People screen (Principle 5: the app clusters, you confirm in batches).
//
// Shows each detected person as a cover-face tile with a count, biggest first.
// Name a person inline; accept/decline "same person?" merge suggestions that
// fold over-split groups back together. Loads on mount (i.e. each time you open
// the tab) and after every action, so it reflects the current clustering.

import { useCallback, useEffect, useState } from "react";
import {
  faceCropUrl,
  getClusters,
  getMergeSuggestions,
  mergeClusters,
  nameCluster,
  type Cluster,
  type MergeSuggestion,
} from "./api";

export default function People() {
  const [clusters, setClusters] = useState<Cluster[]>([]);
  const [suggestions, setSuggestions] = useState<MergeSuggestion[]>([]);
  const [dismissed, setDismissed] = useState<Set<string>>(new Set());
  const [editing, setEditing] = useState<number | null>(null);
  const [draft, setDraft] = useState("");

  const reload = useCallback(() => {
    getClusters().then(setClusters).catch(() => {});
    getMergeSuggestions().then(setSuggestions).catch(() => {});
  }, []);

  useEffect(() => {
    reload();
  }, [reload]);

  const startEdit = (c: Cluster) => {
    setEditing(c.cluster_id);
    setDraft(c.name ?? "");
  };
  const commitEdit = (clusterId: number) => {
    nameCluster(clusterId, draft.trim())
      .then(reload)
      .catch(() => {});
    setEditing(null);
  };

  const doMerge = (s: MergeSuggestion) => {
    mergeClusters(s.into, s.from)
      .then(reload)
      .catch(() => {});
  };
  const decline = (s: MergeSuggestion) => {
    setDismissed((d) => new Set(d).add(`${s.into}-${s.from}`));
  };

  const suggestion = suggestions.find((s) => !dismissed.has(`${s.into}-${s.from}`));

  if (clusters.length === 0) {
    return (
      <div className="empty">
        <p>No people found yet.</p>
        <p className="muted">
          Faces are detected in the background as your photos are indexed. Once a
          few are found, the people you photograph most will show up here to name.
        </p>
      </div>
    );
  }

  return (
    <div className="people-scroll">
      {suggestion && (
        <div className="merge-card">
          <div className="merge-faces">
            <img className="mface" src={faceCropUrl(suggestion.into_cover)} alt="" draggable={false} />
            <span className="mplus">+</span>
            <img className="mface" src={faceCropUrl(suggestion.from_cover)} alt="" draggable={false} />
          </div>
          <div className="merge-text">
            These look like the same person — merge
            {suggestion.into_name ? <> into <b>{suggestion.into_name}</b></> : null}?
          </div>
          <button className="pick-btn" onClick={() => doMerge(suggestion)}>
            Merge
          </button>
          <button className="ghost-btn" onClick={() => decline(suggestion)}>
            Not the same
          </button>
        </div>
      )}

      <div className="people-grid">
        {clusters.map((c) => (
          <div className="ptile" key={c.cluster_id}>
            <img
              className="pavatar"
              src={faceCropUrl(c.cover_face_id)}
              alt={c.name ?? "Unnamed person"}
              draggable={false}
            />
            {editing === c.cluster_id ? (
              <input
                className="pname-input"
                autoFocus
                value={draft}
                placeholder="Name"
                onChange={(e) => setDraft(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") commitEdit(c.cluster_id);
                  else if (e.key === "Escape") setEditing(null);
                }}
                onBlur={() => commitEdit(c.cluster_id)}
              />
            ) : c.name ? (
              <button className="pname" onClick={() => startEdit(c)}>
                {c.name}
              </button>
            ) : (
              <button className="paddname" onClick={() => startEdit(c)}>
                + Add name
              </button>
            )}
            <div className="pcount">{c.count.toLocaleString()} {c.count === 1 ? "photo" : "photos"}</div>
          </div>
        ))}
      </div>
    </div>
  );
}
