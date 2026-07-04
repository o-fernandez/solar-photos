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

import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import PersonView from "./PersonView";
import ReviewFocus from "./ReviewFocus";
import {
  faceCropUrl,
  getClusterGeneration,
  getClusters,
  getFaceProgress,
  getIdentityGrowth,
  getReviewQueue,
  mergeClusters,
  nameCluster,
  onClusterProgress,
  onFaceProgress,
  type Cluster,
  type FaceProgress,
  type IdentityGrowth,
  type ReviewQueue,
} from "./api";
import { usePickerNav } from "./pickerNav";

// While the library is still being scanned, the incremental assign path spawns a
// swarm of 1–3-photo fragments that mostly vanish after consolidation — pure noise
// to watch. So mid-sweep we show only people we're confident are real (named, or
// already large) behind a progress readout, and reveal the full grid once settled.
// Once settled, a gentler floor keeps singletons out of the main grid but reachable
// in a "more" section (over-splitting leaves a real tail the magnet folds back in).
const SWEEP_FLOOR = 8; // min photos for an *unnamed* cluster to show mid-sweep
const SETTLED_FLOOR = 2; // below this, settled clusters move to the "more" section
const MID_SWEEP_REFRESH_MS = 20_000; // how often to re-pull the grid while scanning
// The "more" section reveals tiles a page at a time: purity-bias means thousands
// of small groups is the NORMAL settled state, and mounting them all at once
// (each firing a face-crop request) chokes the DOM at scale (Principle 6).
const MORE_PAGE = 120;

// A few example faces from the queue's top item, for the Review entry card.
function queueFaces(q: ReviewQueue): number[] {
  const it = q.items[0];
  if (!it) return [];
  switch (it.kind) {
    case "strong_batch":
      return it.groups
        .map((g) => g.face_id)
        .filter((f): f is number => f != null)
        .slice(0, 4);
    case "maybe":
      return it.group.face_id != null ? [it.group.face_id] : [];
    case "who_is_this":
      return it.group_faces.slice(0, 4);
    case "pairwise":
      return [...it.into_faces.slice(0, 2), ...it.from_faces.slice(0, 2)];
    case "same_photo_twin":
      return it.pairs.length > 0 ? [it.pairs[0].face_a, it.pairs[0].face_b] : [];
  }
}

function People({
  focusClusterId = null,
  onFocusConsumed,
}: {
  // When set (e.g. from the new-person toast), jump to this person and open their
  // name field. Consumed once, then cleared via onFocusConsumed.
  focusClusterId?: number | null;
  onFocusConsumed?: () => void;
} = {}) {
  const [clusters, setClusters] = useState<Cluster[]>([]);
  const [growth, setGrowth] = useState<IdentityGrowth[]>([]);
  // The unified review queue (all suggestion engines, payoff-sorted) and whether
  // the focus-review session is open.
  const [queue, setQueue] = useState<ReviewQueue | null>(null);
  const [reviewing, setReviewing] = useState(false);
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
  // Find-a-person filter — a hundred named people don't fit in a scroll hunt (P6).
  const [query, setQuery] = useState("");
  // True while a background re-cluster is rebuilding people.
  const [reorganizing, setReorganizing] = useState(false);
  // Whether the face sweep is still running — a boolean, deliberately: the raw
  // per-photo progress events stream constantly while cloud photos trickle in,
  // and holding the counts in state re-rendered the entire tile grid per event
  // (the "hover lags by seconds" bug). Same-value sets bail out render-free.
  const [sweeping, setSweeping] = useState(false);
  // How many of the settled "more" (small/singleton) tiles are revealed (0 = collapsed).
  const [moreShown, setMoreShown] = useState(0);
  // Latest `editing` + last mid-sweep reload time, read inside the progress
  // subscription without making it re-subscribe on every keystroke.
  const editingRef = useRef<number | null>(editing);
  editingRef.current = editing;
  const lastReloadRef = useRef(0);
  // The clustering generation the loaded cluster ids belong to. Passed into
  // naming/merging so the backend refuses an action whose id outlived a
  // re-cluster (naming confirms the whole cluster — the wrong one, durably).
  const genRef = useRef(0);

  const reload = useCallback(() => {
    // All three are instant reads now — the heavy suggestion passes run in the
    // background when clustering settles and are served from a cached snapshot.
    getClusters().then(setClusters).catch(() => {});
    getIdentityGrowth().then(setGrowth).catch(() => {});
    getReviewQueue().then(setQueue).catch(() => {});
    getClusterGeneration()
      .then((g) => {
        genRef.current = g;
      })
      .catch(() => {});
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
    const stillSweeping = (p: FaceProgress) => p.eligible > 0 && p.scanned < p.eligible;
    getFaceProgress().then((p) => setSweeping(stillSweeping(p))).catch(() => {});
    const un = onFaceProgress((p) => {
      const s = stillSweeping(p);
      setSweeping(s); // same value → React bails out, no grid re-render
      const now = Date.now();
      if (s && editingRef.current === null && now - lastReloadRef.current > MID_SWEEP_REFRESH_MS) {
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
  const inProgress = reorganizing || sweeping;

  // Named/confirmed people always show. Mid-sweep, unnamed clusters must clear a
  // high bar (large = reliably real); the rest stays hidden behind the readout.
  // Settled, the bar drops and the small remainder goes to an expandable section.
  // Within the visible grid, named people come first, then unnamed — each block
  // ordered biggest-first (backend already sorts by count, so this stays stable).
  // A search query overrides all of it: just the named people who match.
  const { visible, tail } = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (q) {
      return {
        visible: clusters
          .filter((c) => c.name && c.name.toLowerCase().includes(q))
          .sort((a, b) => b.count - a.count),
        tail: [] as Cluster[],
      };
    }
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
  }, [clusters, inProgress, query]);
  const namedCount = useMemo(() => clusters.filter((c) => c.name).length, [clusters]);

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

  // The tile being renamed and its live suggestions — lifted out of renderTile so
  // the keyboard-nav hook has a single home (only one tile edits at a time).
  const editingCluster =
    editing != null ? clusters.find((c) => c.cluster_id === editing) ?? null : null;
  const editMatches = editingCluster ? nameMatches(editingCluster) : [];
  // Enter commits the typed name; ↑/↓ opt into the merge suggestions first.
  const editNav = usePickerNav(
    editMatches.length,
    (i) => {
      if (editingCluster) mergeInto(editingCluster, editMatches[i]);
    },
    { startUnselected: true },
  );

  // Fold this group into an existing person (chosen from the suggestions, or typed
  // as an exact name match). The named person survives; the growth card then offers
  // to pull in any remaining look-alikes (Direction B's batch fold).
  const mergeInto = (self: Cluster, target: Cluster) => {
    setEditing(null);
    // On refusal (stale generation — people were reorganized under us), reload so
    // the grid shows current ids instead of silently looking like a no-op.
    mergeClusters(target.cluster_id, self.cluster_id, genRef.current)
      .then(reload)
      .catch(() => reload());
  };

  // Enter/blur on the name field: an exact match to an existing person merges; any
  // other text names (or, when empty, clears) this group.
  const commitEdit = (self: Cluster) => {
    const name = draft.trim();
    setEditing(null);
    const match = name ? exactNameMatch(self, name) : undefined;
    if (match) {
      markHintDone();
      mergeClusters(match.cluster_id, self.cluster_id, genRef.current)
        .then(reload)
        .catch(() => reload());
      return;
    }
    if (name || self.name) {
      if (name) markHintDone();
      nameCluster(self.cluster_id, name, genRef.current)
        .then(reload)
        .catch(() => reload());
    }
  };

  // Everything reviewable lives in the unified queue; the banner is just the door.
  const queueReady = queue != null && queue.items.length > 0;
  const queuePhotos = useMemo(
    () => (queue ? queue.items.reduce((n, i) => n + i.photos, 0) : 0),
    [queue],
  );
  // Which people have a review tail waiting, keyed by their tile's cluster id (the
  // identity's largest cluster = the growth card's fold-in target). Drives the tile
  // badge and the review section passed into that person's page.
  const reviewByCluster = useMemo(() => {
    const m = new Map<number, IdentityGrowth>();
    for (const g of growth) if (g.maybe.length > 0) m.set(g.into, g);
    return m;
  }, [growth]);

  // The whole tile opens the person (what a new user tries first); renaming lives
  // behind a hover pencil (named) or a hover "+ Add name" (unnamed) that stop the
  // click from navigating. The name field itself swallows clicks the same way.
  const renderTile = (c: Cluster) => {
    const review = reviewByCluster.get(c.cluster_id);
    return (
    <div
      className="ptile"
      key={c.cluster_id}
      role="button"
      title={c.name ? `See ${c.name}` : "See this person"}
      onClick={() => {
        if (editing !== c.cluster_id) setSelected(c);
      }}
    >
      <div className="pavatar-wrap">
        <img
          className="pavatar"
          src={faceCropUrl(c.cover_face_id)}
          alt={c.name ?? "Unnamed person"}
          draggable={false}
        />
        {review && (
          <span className="pbadge" title={`${review.maybe.length} groups to review`}>
            {review.maybe.length} to review
          </span>
        )}
      </div>
      {editing === c.cluster_id ? (
        <div className="pname-combo" onClick={(e) => e.stopPropagation()}>
          <input
            className="pname-input"
            autoFocus
            value={draft}
            placeholder="Name"
            onChange={(e) => {
              setDraft(e.target.value);
              editNav.resetHighlight();
            }}
            onKeyDown={(e) => {
              if (e.key === "Escape") setEditing(null);
              else if (editNav.onNavKey(e)) return;
              else if (e.key === "Enter") commitEdit(c);
            }}
            onBlur={() => commitEdit(c)}
          />
          {editMatches.length > 0 && (
            // preventDefault keeps the input from blurring (and commit-naming the
            // group) before a suggestion click runs its merge.
            <ul className="name-suggest" onMouseDown={(e) => e.preventDefault()}>
              <li className="name-suggest-head">Add to an existing person</li>
              {editMatches.map((m, i) => (
                <li
                  key={m.cluster_id}
                  className={`name-suggest-item${editNav.highlight === i ? " hi" : ""}`}
                  onMouseEnter={() => editNav.setHighlight(i)}
                  onClick={() => mergeInto(c, m)}
                >
                  <img className="ns-face" src={faceCropUrl(m.cover_face_id)} alt="" draggable={false} />
                  <span className="ns-name">{m.name}</span>
                  <span className="ns-count">{m.count.toLocaleString()}</span>
                </li>
              ))}
            </ul>
          )}
        </div>
      ) : c.name ? (
        <span className="pname-row">
          <span className="pname">{c.name}</span>
          <button
            className="pname-edit"
            aria-label={`Rename ${c.name}`}
            title="Rename"
            onClick={(e) => {
              e.stopPropagation();
              startEdit(c);
            }}
          >
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <path d="M17 3a2.85 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z" />
            </svg>
          </button>
        </span>
      ) : (
        <button
          className="paddname"
          onClick={(e) => {
            e.stopPropagation();
            startEdit(c);
          }}
        >
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
            ? {
                into: review.into,
                name: review.name,
                candidates: review.maybe,
                generation: review.generation,
              }
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
      ) : !hintDone && queueReady ? (
        <div className="reorg-banner people-banner">
          <button className="banner-x" aria-label="Dismiss" title="Dismiss" onClick={markHintDone}>
            ✕
          </button>
          <span className="pb-title">All faces scanned</span>
          <span className="pb-sub">
            Name a few people, then hit Review — Solar asks its best questions one at a
            time, biggest groups first.
          </span>
        </div>
      ) : null}
      {queueReady && (
        <div className="merge-card grow-card review-entry">
          <div className="merge-faces">
            <div className="mside">
              {queueFaces(queue!).map((id) => (
                <img key={id} className="mface" src={faceCropUrl(id)} alt="" draggable={false} />
              ))}
            </div>
          </div>
          <div className="merge-text">
            <b>{queue!.items.length.toLocaleString()}</b>{" "}
            {queue!.items.length === 1 ? "question" : "questions"} to review (
            {queuePhotos.toLocaleString()} {queuePhotos === 1 ? "photo" : "photos"}) — biggest
            first, one at a time. Smaller one-photo questions wait on each person's page.
          </div>
          <button
            className="pick-btn"
            onClick={() => {
              markHintDone();
              setReviewing(true);
            }}
          >
            Review
          </button>
        </div>
      )}
      {reviewing && queue && (
        <ReviewFocus
          queue={queue}
          onClose={() => {
            setReviewing(false);
            reload();
          }}
        />
      )}

      {namedCount >= 8 && (
        <div className="people-tools">
          <input
            className="people-search"
            type="search"
            placeholder="Find a person"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") setQuery("");
            }}
          />
        </div>
      )}

      {visible.length > 0 ? (
        <div className="people-grid">{visible.map(renderTile)}</div>
      ) : query.trim() ? (
        <p className="muted">No one named “{query.trim()}”.</p>
      ) : sweeping ? (
        <p className="muted">
          Finding your people — the first will show up here as your library is scanned.
        </p>
      ) : null}

      {tail.length > 0 && (
        <>
          <button
            className="more-toggle"
            onClick={() => setMoreShown((n) => (n === 0 ? MORE_PAGE : 0))}
          >
            {moreShown > 0 ? "Hide" : "More"} — {tail.length.toLocaleString()} small{" "}
            {tail.length === 1 ? "group" : "groups"}
          </button>
          {moreShown > 0 && (
            <>
              <div className="people-grid">{tail.slice(0, moreShown).map(renderTile)}</div>
              {moreShown < tail.length && (
                <button className="more-toggle" onClick={() => setMoreShown((n) => n + MORE_PAGE)}>
                  Show {Math.min(MORE_PAGE, tail.length - moreShown).toLocaleString()} more —{" "}
                  {(tail.length - moreShown).toLocaleString()} left
                </button>
              )}
            </>
          )}
        </>
      )}
    </div>
  );
}

// Memoized: App re-renders on background-progress ticks (hairline, counters), and
// without this every tick re-rendered the whole tile grid with it.
export default memo(People);
