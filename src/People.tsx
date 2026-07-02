// The People screen (Principle 5: the app clusters, you confirm in batches).
//
// Shows each detected person as a cover-face tile with a count, biggest first.
// Name a person inline — the field suggests existing people as you type, and
// naming a group the same as someone (picked from the list, or an exact match)
// folds it into that person instead of making a duplicate, keeping each name
// unique. Naming then arms the "N more groups look like <name>" growth card,
// which pulls in the remaining look-alikes in one batch. Also accept/decline
// "same person?" merge suggestions that fold over-split groups back together.
// Loads on mount (i.e. each time you open the tab) and after every action, so
// it reflects the current clustering.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import PersonView from "./PersonView";
import {
  absorbClusters,
  faceCropUrl,
  getClusters,
  getFaceProgress,
  getIdentityGrowth,
  getMergeSuggestions,
  mergeClusters,
  nameCluster,
  onClusterProgress,
  onFaceProgress,
  rejectMerge,
  type Cluster,
  type FaceProgress,
  type IdentityGrowth,
  type MergeSuggestion,
} from "./api";

// While the library is still being scanned, the incremental assign path spawns a
// swarm of 1–3-photo fragments that mostly vanish after consolidation — pure noise
// to watch. So mid-sweep we show only people we're confident are real (named, or
// already large) behind a progress readout, and reveal the full grid once settled.
// Once settled, a gentler floor keeps singletons out of the main grid but reachable
// in a "more" section (over-splitting leaves a real tail the magnet folds back in).
const SWEEP_FLOOR = 8; // min photos for an *unnamed* cluster to show mid-sweep
const SETTLED_FLOOR = 2; // below this, settled clusters move to the "more" section
const MID_SWEEP_REFRESH_MS = 20_000; // how often to re-pull the grid while scanning

export default function People({
  focusClusterId = null,
  onFocusConsumed,
}: {
  // When set (e.g. from the new-person toast), jump to this person and open their
  // name field. Consumed once, then cleared via onFocusConsumed.
  focusClusterId?: number | null;
  onFocusConsumed?: () => void;
} = {}) {
  const [clusters, setClusters] = useState<Cluster[]>([]);
  const [suggestions, setSuggestions] = useState<MergeSuggestion[]>([]);
  const [growth, setGrowth] = useState<IdentityGrowth[]>([]);
  const [dismissed, setDismissed] = useState<Set<string>>(new Set());
  const [dismissedGrowth, setDismissedGrowth] = useState<Set<number>>(new Set());
  // Maybe-tail chips the user has already judged (by cluster id), hidden in place so
  // the row doesn't reflow mid-review. Cleared on every reload (the refetch no longer
  // returns them, so a lingering id would only ever be stale).
  // The onboarding banner is a first-run explainer: retire it for good once the user
  // has acted on a suggestion (merged or named someone), so it stops being chrome.
  const [hintDone, setHintDone] = useState<boolean>(
    () => localStorage.getItem("solar.suggestHintDone") === "1",
  );
  const markHintDone = () => {
    localStorage.setItem("solar.suggestHintDone", "1");
    setHintDone(true);
  };
  const [editing, setEditing] = useState<number | null>(null);
  const [draft, setDraft] = useState("");
  // The person whose page is open, in place of the people-grid (null = grid).
  const [selected, setSelected] = useState<Cluster | null>(null);
  // True while a background re-cluster is rebuilding people.
  const [reorganizing, setReorganizing] = useState(false);
  // Sweep progress (null until first read); drives the "Finding people…" readout
  // and decides whether we're still in the noisy mid-sweep phase.
  const [faceProg, setFaceProg] = useState<FaceProgress | null>(null);
  // Whether the settled "more" (small/singleton) section is expanded.
  const [showMore, setShowMore] = useState(false);
  // Latest `editing` + last mid-sweep reload time, read inside the progress
  // subscription without making it re-subscribe on every keystroke.
  const editingRef = useRef<number | null>(editing);
  editingRef.current = editing;
  const lastReloadRef = useRef(0);

  const reload = useCallback(() => {
    // Paint the people grid first. The suggestion passes (merge-evidence graph +
    // per-identity matching) are heavy and hold the DB lock while they compute, so
    // running them inline freezes the tab switch — defer them a tick so the grid
    // shows immediately and the prompts fill in just after.
    getClusters().then(setClusters).catch(() => {});
    setTimeout(() => {
      getMergeSuggestions().then(setSuggestions).catch(() => {});
      getIdentityGrowth().then(setGrowth).catch(() => {});
    }, 50);
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

  // Track sweep progress so the grid knows when the library has settled, and
  // surface newly-qualified people as scanning proceeds instead of freezing the
  // grid until 100%. We re-pull the grid at most once per MID_SWEEP_REFRESH_MS so
  // the long tail of per-photo ticks doesn't reflow constantly (Principle 2) — and
  // the `count >= SWEEP_FLOOR` floor keeps each refresh additive: only confident,
  // real-sized clusters appear, never the singleton churn. Skipped while you're
  // naming someone, so a refresh never yanks the input out from under you.
  useEffect(() => {
    getFaceProgress().then(setFaceProg).catch(() => {});
    const un = onFaceProgress((p) => {
      setFaceProg(p);
      const stillSweeping = p.eligible > 0 && p.scanned < p.eligible;
      const now = Date.now();
      if (
        stillSweeping &&
        editingRef.current === null &&
        now - lastReloadRef.current > MID_SWEEP_REFRESH_MS
      ) {
        lastReloadRef.current = now;
        reload();
      }
    });
    return () => {
      un.then((f) => f());
    };
  }, [reload]);

  // "Still working" = a re-cluster is running, or the sweep hasn't caught up. Same
  // condition the backend uses to gate merge prompts (suggestions_ready).
  const sweeping = !!faceProg && faceProg.eligible > 0 && faceProg.scanned < faceProg.eligible;
  const inProgress = reorganizing || sweeping;

  // Named/confirmed people always show. Mid-sweep, unnamed clusters must clear a
  // high bar (large = reliably real); the rest stays hidden behind the readout.
  // Settled, the bar drops and the small remainder goes to an expandable section.
  // Within the visible grid, named people come first, then unnamed — each block
  // ordered biggest-first (backend already sorts by count, so this stays stable).
  const { visible, tail } = useMemo(() => {
    const isReal = (c: Cluster) => c.name != null;
    const namedFirst = (a: Cluster, b: Cluster) =>
      (a.name != null ? 0 : 1) - (b.name != null ? 0 : 1) || b.count - a.count;
    if (inProgress) {
      return {
        visible: clusters.filter((c) => isReal(c) || c.count >= SWEEP_FLOOR).sort(namedFirst),
        tail: [] as Cluster[],
      };
    }
    return {
      visible: clusters.filter((c) => isReal(c) || c.count >= SETTLED_FLOOR).sort(namedFirst),
      tail: clusters.filter((c) => !isReal(c) && c.count < SETTLED_FLOOR),
    };
  }, [clusters, inProgress]);

  // Arriving from the new-person toast: show the grid and open this person's name
  // field straight away, then tell the parent we've consumed the request.
  useEffect(() => {
    if (focusClusterId == null) return;
    setSelected(null);
    setEditing(focusClusterId);
    setDraft("");
    onFocusConsumed?.();
  }, [focusClusterId, onFocusConsumed]);

  const startEdit = (c: Cluster) => {
    setEditing(c.cluster_id);
    setDraft(c.name ?? "");
  };

  // The named people whose name contains what you're typing (excluding the one
  // you're editing) — the merge-into-an-existing-person suggestions. Biggest first.
  const nameMatches = (self: Cluster): Cluster[] => {
    const q = draft.trim().toLowerCase();
    if (!q) return [];
    return clusters
      .filter((c) => c.cluster_id !== self.cluster_id && c.name && c.name.toLowerCase().includes(q))
      .sort((a, b) => b.count - a.count)
      .slice(0, 5);
  };
  // The existing person whose name is *exactly* what you typed (case-insensitive) —
  // the signal that naming should merge rather than create a duplicate tile.
  const exactNameMatch = (self: Cluster, name: string): Cluster | undefined => {
    const q = name.trim().toLowerCase();
    if (!q) return undefined;
    return clusters.find(
      (c) => c.cluster_id !== self.cluster_id && c.name != null && c.name.toLowerCase() === q,
    );
  };

  // Fold this group into an existing person (chosen from the suggestions, or typed
  // as an exact name match). The named person survives; the growth card then offers
  // to pull in any remaining look-alikes (Direction B's batch fold).
  const mergeInto = (self: Cluster, target: Cluster) => {
    setEditing(null);
    mergeClusters(target.cluster_id, self.cluster_id)
      .then(reload)
      .catch(() => {});
  };

  // Enter/blur on the name field: an exact match to an existing person merges; any
  // other text names (or, when empty, clears) this group.
  const commitEdit = (self: Cluster) => {
    const name = draft.trim();
    setEditing(null);
    const match = name ? exactNameMatch(self, name) : undefined;
    if (match) {
      markHintDone();
      mergeClusters(match.cluster_id, self.cluster_id).then(reload).catch(() => {});
      return;
    }
    if (name || self.name) {
      if (name) markHintDone();
      nameCluster(self.cluster_id, name).then(reload).catch(() => {});
    }
  };

  const doMerge = (s: MergeSuggestion) => {
    markHintDone();
    mergeClusters(s.into, s.from)
      .then(reload)
      .catch(() => {});
  };
  const decline = (s: MergeSuggestion) => {
    // Persist a cannot-link so it never returns (not just this session), then hide it.
    rejectMerge(s.into, s.from).catch(() => {});
    setDismissed((d) => new Set(d).add(`${s.into}-${s.from}`));
  };

  // Bulk-fold every strong match into the person in one action, then reload so the
  // grid counts (and any remaining review tail) reflect it.
  const mergeStrong = (g: IdentityGrowth) => {
    markHintDone();
    absorbClusters(g.into, g.strong_clusters)
      .then(reload)
      .catch(() => {});
  };
  const declineGrowth = (g: IdentityGrowth) => {
    setDismissedGrowth((d) => new Set(d).add(g.identity_id));
  };

  // The banner now carries only the *confident* batch. The less-certain tail moved to
  // each person's own page (reached via the "N to review" badge on their tile), where
  // there's room and, crucially, context — you're looking at that person. So the
  // banner growth card only appears when there's a strong batch to fold in.
  //
  // Both the growth ("N are a strong match") and pairwise ("same person?") tracks
  // unlock on the same "scan finished" gate; show one at a time, growth first (higher
  // precision, clears more per click), pairwise only when there's no growth to offer.
  const grow = growth.find(
    (g) => !dismissedGrowth.has(g.identity_id) && g.strong_clusters.length > 0,
  );
  const suggestion = grow
    ? undefined
    : suggestions.find((s) => !dismissed.has(`${s.into}-${s.from}`));
  // Which people have a review tail waiting, keyed by their tile's cluster id (the
  // identity's largest cluster = the growth card's fold-in target). Drives the tile
  // badge and the review section passed into that person's page.
  const reviewByCluster = useMemo(() => {
    const m = new Map<number, IdentityGrowth>();
    for (const g of growth) if (g.maybe.length > 0) m.set(g.into, g);
    return m;
  }, [growth]);

  const renderTile = (c: Cluster) => {
    const review = reviewByCluster.get(c.cluster_id);
    return (
    <div className="ptile" key={c.cluster_id}>
      <div className="pavatar-wrap" onClick={() => setSelected(c)}>
        <img
          className="pavatar"
          src={faceCropUrl(c.cover_face_id)}
          alt={c.name ?? "Unnamed person"}
          title={c.name ? `See ${c.name}` : "See this person"}
          draggable={false}
        />
        {review && (
          <span className="pbadge" title={`${review.maybe.length} groups to review`}>
            {review.maybe.length} to review
          </span>
        )}
      </div>
      {editing === c.cluster_id ? (
        <div className="pname-combo">
          <input
            className="pname-input"
            autoFocus
            value={draft}
            placeholder="Name"
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") commitEdit(c);
              else if (e.key === "Escape") setEditing(null);
            }}
            onBlur={() => commitEdit(c)}
          />
          {(() => {
            const matches = nameMatches(c);
            if (matches.length === 0) return null;
            // preventDefault keeps the input from blurring (and commit-naming the
            // group) before a suggestion click runs its merge.
            return (
              <ul className="name-suggest" onMouseDown={(e) => e.preventDefault()}>
                <li className="name-suggest-head">Add to an existing person</li>
                {matches.map((m) => (
                  <li
                    key={m.cluster_id}
                    className="name-suggest-item"
                    onClick={() => mergeInto(c, m)}
                  >
                    <img className="ns-face" src={faceCropUrl(m.cover_face_id)} alt="" draggable={false} />
                    <span className="ns-name">{m.name}</span>
                    <span className="ns-count">{m.count.toLocaleString()}</span>
                  </li>
                ))}
              </ul>
            );
          })()}
        </div>
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
    );
  };

  // A person's page replaces the grid; returning reloads so counts reflect any
  // "not this person" corrections and renames made there.
  if (selected) {
    const review = reviewByCluster.get(selected.cluster_id);
    return (
      <PersonView
        cluster={selected}
        review={
          review
            ? { into: review.into, name: review.name, candidates: review.maybe }
            : undefined
        }
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
      {sweeping ? (
        // While the sweep is running the backend fires periodic re-clusters as the
        // in-flight queue drains. Those flip `reorganizing` on and off, but they're an
        // internal detail — keep one stable "Still finding people" banner across the
        // whole sweep instead of ping-ponging to "Reorganizing…" on each one.
        <div className="reorg-banner people-banner">
          <span className="pb-title">Still finding people</span>
          <span className="pb-sub">
            You’ll see the same person in a few separate groups while scanning — that’s
            expected. Name a few favorites now if you like; when scanning finishes, Solar
            groups the rest together for you.
          </span>
        </div>
      ) : reorganizing ? (
        <div className="reorg-banner">Reorganizing people…</div>
      ) : !hintDone && (grow || suggestion) ? (
        <div className="reorg-banner people-banner">
          <button className="banner-x" aria-label="Dismiss" title="Dismiss" onClick={markHintDone}>
            ✕
          </button>
          <span className="pb-title">All faces scanned</span>
          <span className="pb-sub">
            Name a few people and accept the suggestions below — similar groups fold in
            automatically.
          </span>
        </div>
      ) : null}
      {grow && (
        <div className="merge-card grow-card">
          <div className="merge-faces">
            <div className="mside">
              {grow.anchor_faces.map((id) => (
                <img key={id} className="mface" src={faceCropUrl(id)} alt="" draggable={false} />
              ))}
            </div>
            <span className="mplus">+</span>
            <div className="mside">
              {grow.strong_faces.map((id) => (
                <img key={id} className="mface" src={faceCropUrl(id)} alt="" draggable={false} />
              ))}
            </div>
          </div>
          <div className="merge-text">
            {grow.maybe.length > 0 ? (
              <>
                <b>
                  {grow.strong_clusters.length.toLocaleString()}{" "}
                  {grow.strong_clusters.length === 1 ? "group" : "groups"}
                </b>{" "}
                {grow.strong_clusters.length === 1 ? "is a strong match" : "are a strong match"} for{" "}
                <b>{grow.name}</b> ({grow.strong_photos.toLocaleString()}{" "}
                {grow.strong_photos === 1 ? "photo" : "photos"}).
              </>
            ) : (
              <>
                {grow.strong_clusters.length.toLocaleString()}{" "}
                {grow.strong_clusters.length === 1 ? "group" : "groups"} (
                {grow.strong_photos.toLocaleString()}{" "}
                {grow.strong_photos === 1 ? "photo" : "photos"}) look like <b>{grow.name}</b> — merge
                them all?
              </>
            )}
          </div>
          <button className="pick-btn" onClick={() => mergeStrong(grow)}>
            {grow.maybe.length > 0
              ? `Merge ${grow.strong_clusters.length.toLocaleString()}`
              : "Merge all"}
          </button>
          <button className="ghost-btn" onClick={() => declineGrowth(grow)}>
            Not now
          </button>
        </div>
      )}
      {suggestion && (
        <div className="merge-card">
          <div className="merge-faces">
            <div className="mside">
              {suggestion.into_faces.map((id) => (
                <img key={id} className="mface" src={faceCropUrl(id)} alt="" draggable={false} />
              ))}
            </div>
            <span className="mplus">+</span>
            <div className="mside">
              {suggestion.from_faces.map((id) => (
                <img key={id} className="mface" src={faceCropUrl(id)} alt="" draggable={false} />
              ))}
            </div>
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

      {visible.length > 0 ? (
        <div className="people-grid">{visible.map(renderTile)}</div>
      ) : sweeping ? (
        <p className="muted">
          Finding your people — the first will show up here as your library is scanned.
        </p>
      ) : null}

      {tail.length > 0 && (
        <>
          <button className="more-toggle" onClick={() => setShowMore((s) => !s)}>
            {showMore ? "Hide" : "More"} — {tail.length.toLocaleString()} small{" "}
            {tail.length === 1 ? "group" : "groups"}
          </button>
          {showMore && <div className="people-grid">{tail.map(renderTile)}</div>}
        </>
      )}
    </div>
  );
}
