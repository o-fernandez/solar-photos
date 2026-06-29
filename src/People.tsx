// The People screen (Principle 5: the app clusters, you confirm in batches).
//
// Shows each detected person as a cover-face tile with a count, biggest first.
// Name a person inline; accept/decline "same person?" merge suggestions that
// fold over-split groups back together. Loads on mount (i.e. each time you open
// the tab) and after every action, so it reflects the current clustering.

import { useCallback, useEffect, useState } from "react";
import PersonView from "./PersonView";
import {
  faceCropUrl,
  getClusters,
  getMergeSuggestions,
  mergeClusters,
  nameCluster,
  onClusterProgress,
  type Cluster,
  type MergeSuggestion,
} from "./api";

export default function People() {
  const [clusters, setClusters] = useState<Cluster[]>([]);
  const [suggestions, setSuggestions] = useState<MergeSuggestion[]>([]);
  const [dismissed, setDismissed] = useState<Set<string>>(new Set());
  const [editing, setEditing] = useState<number | null>(null);
  const [draft, setDraft] = useState("");
  // The person whose page is open, in place of the people-grid (null = grid).
  const [selected, setSelected] = useState<Cluster | null>(null);
  // True while a background re-cluster is rebuilding people.
  const [reorganizing, setReorganizing] = useState(false);

  const reload = useCallback(() => {
    getClusters().then(setClusters).catch(() => {});
    getMergeSuggestions().then(setSuggestions).catch(() => {});
  }, []);

  useEffect(() => {
    reload();
  }, [reload]);

  // The re-cluster runs in the background. We reload exactly once, when it
  // finishes (`running` → false), so the grid settles in one update instead of
  // reflowing mid-rebuild (Principle 2). A subtle banner shows it's working.
  useEffect(() => {
    const un = onClusterProgress((p) => {
      setReorganizing(p.running);
      if (!p.running) reload();
    });
    return () => {
      un.then((f) => f());
    };
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

  // A person's page replaces the grid; returning reloads so counts reflect any
  // "not this person" corrections and renames made there.
  if (selected) {
    return (
      <PersonView
        cluster={selected}
        onBack={() => {
          setSelected(null);
          reload();
        }}
      />
    );
  }

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
      {reorganizing && (
        <div className="reorg-banner">Reorganizing people…</div>
      )}
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
              title={c.name ? `See ${c.name}` : "See this person"}
              draggable={false}
              onClick={() => setSelected(c)}
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
