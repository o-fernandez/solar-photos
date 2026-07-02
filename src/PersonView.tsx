// One person's page: every photo they're in, newest first — the reason People
// exists ("show me all of Camila"). Reached by tapping a face tile in People.
//
// It's a filtered timeline: the same fixed-cell, virtualized grid as PhotoGrid
// (copied minimal here rather than forking the main grid — none of its
// scan-growth / cloud-on-demand / scrubber machinery applies to a known set),
// the same Lightbox reused with ←/→ scoped to this person, plus a header
// (cover, name, count, date span) and a per-photo "not this person" correction.
//
// Honors the same principles: fixed-size cells so a thumbnail filling in never
// reflows (P2); renders from already-cached thumbnails (P4); virtualized so a
// 4,000-photo person stays smooth (P6).

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import Lightbox from "./Lightbox";
import {
  absorbClusters,
  detachFaces,
  faceCropUrl,
  faceIdsForPhotos,
  getClusters,
  getPersonLooks,
  getPersonPhotos,
  ignoreFaces,
  mergeClusters,
  nameCluster,
  notThisPerson,
  onThumbReady,
  reassignFacesToCluster,
  reassignFacesToNewPerson,
  rejectMerge,
  setVisibleRange,
  thumbUrl,
  undoCorrection,
  STATUS_READY,
  STATUS_DOWNLOADING,
  STATUS_CLOUD,
  STATUS_FAILED,
  type Cluster,
  type CorrectionUndo,
  type GrowthCluster,
  type PersonLook,
  type PhotoRow,
} from "./api";

const GAP = 4; // px between cells (matches the timeline grid)
const TARGET_CELL = 200; // px — desired cell edge; actual size flexes to fill width
const OVERSCAN_ROWS = 4;

// Newest-first insertion point, keeping rows sorted by (ts DESC, id DESC) — the
// same order the backend returns, so an undo drops the photo back where it was.
function insertSorted(rows: PhotoRow[], row: PhotoRow): PhotoRow[] {
  const next = rows.slice();
  let i = next.findIndex((r) => r.ts < row.ts || (r.ts === row.ts && r.id < row.id));
  if (i < 0) i = next.length;
  next.splice(i, 0, row);
  return next;
}

function monthYear(ts: number): string {
  return new Date(ts * 1000).toLocaleDateString(undefined, { month: "short", year: "numeric" });
}

// A just-applied correction the user can still take back: the rows we optimistically
// pulled from the grid, plus the backend token that restores them exactly.
interface PendingUndo {
  rows: PhotoRow[];
  undo: CorrectionUndo;
  label: string;
}

export default function PersonView({
  cluster,
  review,
  onBack,
}: {
  cluster: Cluster;
  // The less-certain look-alike groups the magnet thinks might also be this person —
  // reviewed here, in context, one at a time. `into` is the cluster a "yes" folds
  // into; `generation` is the clustering generation the card was computed at, passed
  // back so the backend can refuse an answer that outlived a re-cluster.
  review?: { into: number; name: string; candidates: GrowthCluster[]; generation: number };
  onBack: () => void;
}) {
  const [rows, setRows] = useState<PhotoRow[]>([]);
  const [loaded, setLoaded] = useState(false);
  // This person's "looks" (appearance sub-clusters) and which one filters the grid.
  const [looks, setLooks] = useState<PersonLook[]>([]);
  const [selectedLook, setSelectedLook] = useState<number | null>(null);
  // Whether the selected look's "move to which person?" picker is open, and its text.
  const [lookPicking, setLookPicking] = useState(false);
  const [lookPickQuery, setLookPickQuery] = useState("");
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");
  const [name, setName] = useState(cluster.name);
  const [viewerIndex, setViewerIndex] = useState<number | null>(null);
  const [undo, setUndo] = useState<PendingUndo | null>(null);
  // Photo ids the user has multi-selected for a bulk correction.
  const [selected, setSelected] = useState<Set<number>>(new Set());
  // Whether the "move to which person?" picker is open, and its typeahead text.
  const [picking, setPicking] = useState(false);
  const [pickQuery, setPickQuery] = useState("");
  // The people to reassign into (named/large groups), loaded once.
  const [people, setPeople] = useState<Cluster[]>([]);

  const scrollRef = useRef<HTMLDivElement>(null);
  // Thumbnail readiness, seeded from row status and kept live via onThumbReady.
  // A ref + tick so a burst of thumb events coalesces into one paint.
  const readyRef = useRef<Set<number>>(new Set());
  const [, setTick] = useState(0);
  const invalidatePending = useRef(false);
  const invalidate = useCallback(() => {
    if (invalidatePending.current) return;
    invalidatePending.current = true;
    requestAnimationFrame(() => {
      invalidatePending.current = false;
      setTick((t) => t + 1);
    });
  }, []);

  // The grid shows the whole person, or just the selected look. Filtering is
  // client-side (we already hold every photo), so a look switch is instant.
  const shown = useMemo(() => {
    if (selectedLook == null || !looks[selectedLook]) return rows;
    const ids = new Set(looks[selectedLook].photo_ids);
    return rows.filter((r) => ids.has(r.id));
  }, [rows, looks, selectedLook]);

  // resolveId reads the latest shown rows without re-subscribing the viewer, so the
  // lightbox's ←/→ stay scoped to whatever the grid is currently showing.
  const rowsRef = useRef<PhotoRow[]>(shown);
  rowsRef.current = shown;
  const resolveId = useCallback(
    (i: number): Promise<number | null> => Promise.resolve(rowsRef.current[i]?.id ?? null),
    [],
  );
  // The full photo set (independent of any look filter) — corrections and undo act on
  // this so they're correct even when the grid is filtered to one look.
  const fullRowsRef = useRef<PhotoRow[]>(rows);
  fullRowsRef.current = rows;

  // Reload this person's photo set from the backend — after a correction made in
  // the open photo (Lightbox) changes who's in it.
  const reloadPhotos = useCallback(() => {
    getPersonPhotos(cluster.cluster_id)
      .then((r) => {
        r.forEach((row) => {
          if (row.status === STATUS_READY) readyRef.current.add(row.id);
        });
        setRows(r);
        setLoaded(true);
      })
      .catch(() => setLoaded(true));
  }, [cluster.cluster_id]);

  // Load (and reload) this person's "looks" — the appearance sub-clusters. Refreshed
  // after any correction, since moving faces changes the grouping (and clears a flag).
  const loadLooks = useCallback(() => {
    getPersonLooks(cluster.cluster_id)
      .then((l) => {
        setLooks(l);
        setSelectedLook((cur) => (cur != null && cur >= l.length ? null : cur));
      })
      .catch(() => {});
  }, [cluster.cluster_id]);

  // Load this person's photos + looks once on mount (the whole set is known — no paging).
  useEffect(() => {
    reloadPhotos();
    loadLooks();
  }, [reloadPhotos, loadLooks]);

  // Fill cells whose thumbnails finish while the page is open.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onThumbReady((d) => {
      if (d.ok) {
        readyRef.current.add(d.id);
        invalidate();
      }
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, [invalidate]);

  // --- Responsive column count: measure the scroll container's width. ---
  const [width, setWidth] = useState(0);
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const ro = new ResizeObserver((entries) => setWidth(entries[0].contentRect.width));
    ro.observe(el);
    setWidth(el.clientWidth);
    return () => ro.disconnect();
  }, [loaded]);

  const total = shown.length;
  const { columns, cellSize, rowHeight, rowCount } = useMemo(() => {
    const w = Math.max(width, TARGET_CELL);
    const cols = Math.max(1, Math.floor((w + GAP) / (TARGET_CELL + GAP)));
    const size = Math.floor((w - GAP * (cols - 1)) / cols);
    return { columns: cols, cellSize: size, rowHeight: size + GAP, rowCount: Math.ceil(total / cols) };
  }, [width, total]);

  const rowVirtualizer = useVirtualizer({
    count: rowCount,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => rowHeight,
    overscan: OVERSCAN_ROWS,
  });
  const virtualRows = rowVirtualizer.getVirtualItems();

  // Prioritize thumbnails for the photos currently on screen (P3). These are
  // local, so this just jumps the local queue; cloud-on-demand doesn't apply.
  const prioritizeTimer = useRef<number | undefined>(undefined);
  useEffect(() => {
    if (virtualRows.length === 0 || columns === 0) return;
    const firstIndex = virtualRows[0].index * columns;
    const lastIndex = Math.min(total - 1, (virtualRows[virtualRows.length - 1].index + 1) * columns - 1);
    if (prioritizeTimer.current) window.clearTimeout(prioritizeTimer.current);
    prioritizeTimer.current = window.setTimeout(() => {
      const ids: number[] = [];
      for (let i = firstIndex; i <= lastIndex; i++) {
        const p = shown[i];
        if (p && !readyRef.current.has(p.id)) ids.push(p.id);
      }
      setVisibleRange(ids).catch(() => {});
    }, 80);
  }, [virtualRows, columns, total, shown]);

  // Existing people whose name contains the draft (merge-into-existing suggestions),
  // and the one that matches it exactly — the signal that naming should merge, not
  // rename. Mirrors the People grid so renaming here behaves the same.
  const nameMatches = (q: string): Cluster[] => {
    const s = q.trim().toLowerCase();
    if (!s) return [];
    return people
      .filter((c) => c.cluster_id !== cluster.cluster_id && c.name && c.name.toLowerCase().includes(s))
      .slice(0, 5);
  };
  const exactNameMatch = (q: string): Cluster | undefined => {
    const s = q.trim().toLowerCase();
    if (!s) return undefined;
    return people.find(
      (c) => c.cluster_id !== cluster.cluster_id && c.name != null && c.name.toLowerCase() === s,
    );
  };
  // Fold this whole person into another (picked, or typed as an exact match), then
  // leave the page — this cluster is now part of the other.
  const mergeThisInto = (target: Cluster) => {
    setEditing(false);
    mergeClusters(target.cluster_id, cluster.cluster_id).then(onBack).catch(() => {});
  };

  const commitName = () => {
    const value = draft.trim();
    setEditing(false);
    const match = value ? exactNameMatch(value) : undefined;
    if (match) {
      mergeThisInto(match);
      return;
    }
    nameCluster(cluster.cluster_id, value).catch(() => {});
    setName(value || null);
  };

  // Review-tail decisions (the "N more might also be this person" band). "Yes" folds
  // the group in and pulls its photos into this page; "no" writes a durable cannot-link
  // so it never returns. Resolved chips hide in place; when the band empties it's gone.
  const [reviewResolved, setReviewResolved] = useState<Set<number>>(new Set());
  const reviewLeft = (review?.candidates ?? []).filter((c) => !reviewResolved.has(c.cluster_id));
  const resolveReview = (c: GrowthCluster, keep: boolean) => {
    if (!review) return;
    setReviewResolved((s) => new Set(s).add(c.cluster_id));
    // "Yes" folds the group in; "No" makes it a durable competitor (its own confirmed
    // identity) so this and other look-alikes get pulled away from this person. The
    // generation check makes a chip that outlived a re-cluster fail instead of acting
    // on whatever cluster now holds its id — un-hide it so the user sees it didn't land.
    (keep
      ? absorbClusters(review.into, [c.cluster_id], review.generation)
      : notThisPerson(review.into, c.cluster_id, review.generation)
    )
      .then(() => {
        if (keep) reloadPhotos();
      })
      .catch(() => {
        setReviewResolved((s) => {
          const next = new Set(s);
          next.delete(c.cluster_id);
          return next;
        });
      });
  };

  // The people you can reassign a chunk *into* — every other person, biggest first.
  // Loaded once; the picker filters it by the typeahead.
  useEffect(() => {
    getClusters().then(setPeople).catch(() => {});
  }, []);

  const toggleSelect = (photoId: number) => {
    setSelected((s) => {
      const next = new Set(s);
      next.has(photoId) ? next.delete(photoId) : next.add(photoId);
      return next;
    });
  };
  const clearSelection = () => {
    setSelected(new Set());
    setPicking(false);
    setPickQuery("");
  };

  // Apply a correction to a set of photos: resolve their faces, optimistically pull
  // the cells (P2 — one update, no reflow under the user), run the backend op, and
  // offer one-level Undo. The selection clears either way.
  const undoTimer = useRef<number | undefined>(undefined);
  const applyCorrection = useCallback(
    async (photoIds: number[], run: (faceIds: number[]) => Promise<CorrectionUndo>, label: string) => {
      if (photoIds.length === 0) return;
      const idSet = new Set(photoIds);
      const removed = fullRowsRef.current.filter((r) => idSet.has(r.id));
      setRows((rs) => rs.filter((r) => !idSet.has(r.id)));
      clearSelection();
      try {
        const faceIds = await faceIdsForPhotos(photoIds, cluster.cluster_id);
        const tok = await run(faceIds);
        setUndo({ rows: removed, undo: tok, label });
        if (undoTimer.current) window.clearTimeout(undoTimer.current);
        undoTimer.current = window.setTimeout(() => setUndo(null), 6000);
        // Moving faces out changes the look grouping (and clears a repair flag).
        loadLooks();
      } catch {
        setRows((rs) => removed.reduce((acc, r) => insertSorted(acc, r), rs));
      }
    },
    [cluster.cluster_id, loadLooks],
  );

  const doUndo = () => {
    if (!undo) return;
    const { rows: removed, undo: tok } = undo;
    setUndo(null);
    undoCorrection(tok)
      .then(() => {
        setRows((rs) => removed.reduce((acc, r) => insertSorted(acc, r), rs));
        loadLooks();
      })
      .catch(() => {});
  };

  const selectedIds = useMemo(() => [...selected], [selected]);
  const moveToPerson = (target: Cluster) =>
    applyCorrection(
      selectedIds,
      (fids) => reassignFacesToCluster(fids, cluster.cluster_id, target.cluster_id),
      `Moved to ${target.name}`,
    );
  const moveToNewPerson = (newName?: string) =>
    applyCorrection(
      selectedIds,
      (fids) => reassignFacesToNewPerson(fids, cluster.cluster_id, newName),
      newName ? `Moved to ${newName}` : "Moved to a new person",
    );
  const ignoreSelected = () =>
    applyCorrection(selectedIds, (fids) => ignoreFaces(fids), "Ignored");
  // "Not [name]" on a multi-selection: detach without saying who they are — each
  // re-homes by appearance (may become several people, or none), not forced together.
  const notThisSelected = () =>
    applyCorrection(selectedIds, (fids) => detachFaces(fids), `Not ${name ?? "this person"}`);

  // Acting on a whole look (the selected swatch). Every look — flagged or not — can be
  // moved to a person you pick, sent to a specific target, or detached back to the
  // unnamed batches. All reuse the reassign+undo path; clear the filter so the result
  // is visible. `endLook` resets the swatch selection + picker afterward.
  const activeLook = selectedLook != null ? looks[selectedLook] : undefined;
  const endLook = () => {
    setSelectedLook(null);
    setLookPicking(false);
    setLookPickQuery("");
  };
  const moveLookToCluster = (targetCluster: number, label: string) => {
    if (!activeLook) return;
    const ids = activeLook.photo_ids;
    endLook();
    applyCorrection(ids, (fids) => reassignFacesToCluster(fids, cluster.cluster_id, targetCluster), label);
  };
  const moveLookToPerson = (target: Cluster) =>
    moveLookToCluster(target.cluster_id, `Moved to ${target.name}`);
  // "+ New person" in the look picker: split the whole look into one fresh person
  // (optionally named) — they ARE all one person, just not any existing one.
  const moveLookToNewPerson = (newName?: string) => {
    if (!activeLook) return;
    const ids = activeLook.photo_ids;
    endLook();
    applyCorrection(
      ids,
      (fids) => reassignFacesToNewPerson(fids, cluster.cluster_id, newName),
      newName ? `Moved to ${newName}` : "Moved to a new person",
    );
  };
  // "Not [name]" on a look: detach without saying who — each re-homes by appearance,
  // not forced together (they may be several different people).
  const notLook = () => {
    if (!activeLook) return;
    const ids = activeLook.photo_ids;
    endLook();
    applyCorrection(ids, (fids) => detachFaces(fids), `Not ${name ?? "this person"}`);
  };
  // "It's actually this person" on a flagged look: record that this person and the
  // suggested other are *different* people (durable cannot-link), which both dismisses
  // this flag and stops the look ever being suggested as them again.
  const keepLook = () => {
    if (!activeLook || activeLook.likely_other_cluster == null) return;
    endLook();
    rejectMerge(cluster.cluster_id, activeLook.likely_other_cluster).then(loadLooks).catch(() => {});
  };

  // Named people other than the one we're viewing, filtered by a typeahead — shared by
  // the multi-select move picker and the per-look move picker.
  const filterPeople = (q: string) =>
    people
      .filter((c) => c.cluster_id !== cluster.cluster_id && c.name)
      .filter((c) => (q.trim() ? c.name!.toLowerCase().includes(q.trim().toLowerCase()) : true))
      .slice(0, 6);
  const pickMatches = useMemo(() => filterPeople(pickQuery), [people, pickQuery, cluster.cluster_id]); // eslint-disable-line react-hooks/exhaustive-deps
  const lookPickMatches = useMemo(() => filterPeople(lookPickQuery), [people, lookPickQuery, cluster.cluster_id]); // eslint-disable-line react-hooks/exhaustive-deps

  const header = (
    <div className="person-header">
      <button className="ghost-btn person-back" onClick={onBack}>
        ‹ Back
      </button>
      <img className="person-avatar" src={faceCropUrl(cluster.cover_face_id)} alt="" draggable={false} />
      <div className="person-meta">
        {editing ? (
          <div className="pname-combo">
            <input
              className="pname-input"
              autoFocus
              value={draft}
              placeholder="Name"
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") commitName();
                else if (e.key === "Escape") setEditing(false);
              }}
              onBlur={commitName}
            />
            {(() => {
              const matches = nameMatches(draft);
              if (matches.length === 0) return null;
              // preventDefault keeps the input from blurring (and rename-committing)
              // before a suggestion click runs its merge.
              return (
                <ul className="name-suggest" onMouseDown={(e) => e.preventDefault()}>
                  <li className="name-suggest-head">Merge into an existing person</li>
                  {matches.map((m) => (
                    <li
                      key={m.cluster_id}
                      className="name-suggest-item"
                      onClick={() => mergeThisInto(m)}
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
        ) : name ? (
          <button
            className="person-name"
            onClick={() => {
              setDraft(name);
              setEditing(true);
            }}
          >
            {name}
          </button>
        ) : (
          <button
            className="paddname person-name"
            onClick={() => {
              setDraft("");
              setEditing(true);
            }}
          >
            + Add name
          </button>
        )}
        <div className="person-sub">
          {rows.length.toLocaleString()} {rows.length === 1 ? "photo" : "photos"}
          {rows.length > 0 && (
            <>
              {" · "}
              {(() => {
                const lo = monthYear(rows[rows.length - 1].ts);
                const hi = monthYear(rows[0].ts);
                return lo === hi ? lo : `${lo} – ${hi}`;
              })()}
            </>
          )}
        </div>
      </div>
    </div>
  );

  return (
    <div className="person-view">
      {header}

      {looks.length > 0 && (
        <div className="person-looks">
          <button
            className={`look look-all${selectedLook == null ? " sel" : ""}`}
            onClick={() => setSelectedLook(null)}
          >
            <span className="look-allmark" aria-hidden="true">▦</span>
            <span className="look-lbl">All</span>
            <span className="look-sub">{rows.length.toLocaleString()}</span>
          </button>
          {looks.map((lk, i) => {
            const flagged = lk.likely_other_name != null;
            return (
              <button
                key={i}
                className={`look${flagged ? " flag" : ""}${selectedLook === i ? " sel" : ""}`}
                title={
                  flagged
                    ? `Might be ${lk.likely_other_name} — click to move`
                    : "Filter to this look, or move it"
                }
                onClick={() => {
                  setSelectedLook(selectedLook === i ? null : i);
                  setLookPicking(false);
                }}
              >
                <img className="look-face" src={faceCropUrl(lk.cover_face_id)} alt="" draggable={false} />
                {flagged ? (
                  <span className="look-flagtag">looks like {lk.likely_other_name}</span>
                ) : (
                  <span className="look-sub">
                    {lk.photos.toLocaleString()} {lk.photos === 1 ? "photo" : "photos"}
                  </span>
                )}
              </button>
            );
          })}
        </div>
      )}

      {activeLook && selected.size === 0 && (
        <div className="look-bar">
          <span className="lb-count">
            {activeLook.photos.toLocaleString()} in this look
          </span>
          {lookPicking ? (
            <div className="sb-picker">
              <input
                className="pname-input"
                autoFocus
                value={lookPickQuery}
                placeholder="Move to which person?"
                onChange={(e) => setLookPickQuery(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Escape") setLookPicking(false);
                  else if (e.key === "Enter" && lookPickQuery.trim())
                    moveLookToNewPerson(lookPickQuery.trim());
                }}
              />
              <ul className="sb-matches">
                {lookPickMatches.map((m) => (
                  <li key={m.cluster_id} className="sb-match" onClick={() => moveLookToPerson(m)}>
                    <img className="ns-face" src={faceCropUrl(m.cover_face_id)} alt="" draggable={false} />
                    <span className="ns-name">{m.name}</span>
                    <span className="ns-count">{m.count.toLocaleString()}</span>
                  </li>
                ))}
                <li
                  className="sb-match sb-new"
                  onClick={() => moveLookToNewPerson(lookPickQuery.trim() || undefined)}
                >
                  + New person{lookPickQuery.trim() ? ` “${lookPickQuery.trim()}”` : ""}
                </li>
              </ul>
            </div>
          ) : activeLook.likely_other_name != null ? (
            // A flagged look: affirm it's this person, accept the suggestion, or pick.
            <>
              <button className="sb-btn" onClick={keepLook} title="This look really is this person">
                It’s {name ?? "this person"}
              </button>
              <button
                className="sb-btn"
                onClick={() =>
                  moveLookToCluster(
                    activeLook.likely_other_cluster!,
                    `Moved to ${activeLook.likely_other_name}`,
                  )
                }
              >
                Move to {activeLook.likely_other_name}
              </button>
              <button className="sb-btn" onClick={() => setLookPicking(true)}>
                Someone else…
              </button>
              <button className="sb-btn ghost" onClick={endLook}>
                Done
              </button>
            </>
          ) : (
            // A genuine look: move it to a person, or detach it back to unnamed.
            <>
              <button className="sb-btn" onClick={() => setLookPicking(true)}>
                Move to…
              </button>
              <button
                className="sb-btn"
                title="Detach — let each face re-cluster where it belongs"
                onClick={notLook}
              >
                Not {name ?? "this person"}
              </button>
              <button className="sb-btn ghost" onClick={endLook}>
                Done
              </button>
            </>
          )}
        </div>
      )}

      {reviewLeft.length > 0 && (
        <div className="person-review">
          <div className="pr-title">
            {reviewLeft.length.toLocaleString()} {reviewLeft.length === 1 ? "group" : "groups"} might
            also be <b>{name ?? review!.name}</b> — check each
          </div>
          <div className="pr-row">
            {reviewLeft.map((c) => (
              <div className="pr-chip" key={c.cluster_id}>
                {c.face_id != null ? (
                  <img className="pr-face" src={faceCropUrl(c.face_id)} alt="" draggable={false} />
                ) : (
                  <div className="pr-face pr-face-blank" />
                )}
                <div className="pr-count">{c.photos.toLocaleString()}</div>
                <div className="pr-yn">
                  <button
                    className="pr-y"
                    title={`Yes — add to ${name ?? review!.name}`}
                    aria-label={`Yes, this is ${name ?? review!.name}`}
                    onClick={() => resolveReview(c, true)}
                  >
                    ✓
                  </button>
                  <button
                    className="pr-n"
                    title="Not the same person"
                    aria-label="Not the same person"
                    onClick={() => resolveReview(c, false)}
                  >
                    ✕
                  </button>
                </div>
              </div>
            ))}
          </div>
        </div>
      )}

      {loaded && total === 0 ? (
        <div className="empty">
          <p>No photos left for this person.</p>
          <button className="ghost-btn" onClick={onBack}>
            ‹ Back to People
          </button>
        </div>
      ) : (
        <div className="grid-wrap">
          <div ref={scrollRef} className="grid-scroll person-scroll">
            <div style={{ height: `${rowVirtualizer.getTotalSize()}px`, width: "100%", position: "relative" }}>
              {virtualRows.map((virtualRow) => {
                const rowStart = virtualRow.index * columns;
                const cells = [];
                for (let c = 0; c < columns; c++) {
                  const index = rowStart + c;
                  if (index >= total) break;
                  const photo = shown[index];
                  const isSelected = selected.has(photo.id);
                  const selecting = selected.size > 0;
                  cells.push(
                    <div
                      key={photo.id}
                      className={`cell person-cell${isSelected ? " selected" : ""}`}
                      role="button"
                      tabIndex={-1}
                      // Once a selection is underway, taps add/remove from it; otherwise
                      // a tap opens the photo. The checkbox always toggles selection.
                      onClick={() => (selecting ? toggleSelect(photo.id) : setViewerIndex(index))}
                      style={{
                        width: cellSize,
                        height: cellSize,
                        marginRight: c < columns - 1 ? GAP : 0,
                        cursor: "pointer",
                      }}
                    >
                      {renderCellContent(photo)}
                      <button
                        className={`person-select${isSelected ? " on" : ""}`}
                        title={isSelected ? "Selected" : "Select"}
                        aria-label={isSelected ? "Deselect photo" : "Select photo"}
                        aria-pressed={isSelected}
                        onClick={(e) => {
                          e.stopPropagation();
                          toggleSelect(photo.id);
                        }}
                      >
                        {isSelected ? "✓" : ""}
                      </button>
                    </div>,
                  );
                }
                return (
                  <div
                    key={virtualRow.index}
                    className="grid-row"
                    style={{
                      position: "absolute",
                      top: 0,
                      left: 0,
                      width: "100%",
                      height: cellSize,
                      transform: `translateY(${virtualRow.start}px)`,
                    }}
                  >
                    {cells}
                  </div>
                );
              })}
            </div>
          </div>
        </div>
      )}

      {selected.size > 0 && (
        <div className="select-bar">
          <span className="sb-count">{selected.size} selected</span>
          {picking ? (
            <div className="sb-picker">
              <input
                className="pname-input"
                autoFocus
                value={pickQuery}
                placeholder="Move to which person?"
                onChange={(e) => setPickQuery(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Escape") setPicking(false);
                  else if (e.key === "Enter" && pickQuery.trim()) moveToNewPerson(pickQuery.trim());
                }}
              />
              <ul className="sb-matches">
                {pickMatches.map((m) => (
                  <li key={m.cluster_id} className="sb-match" onClick={() => moveToPerson(m)}>
                    <img className="ns-face" src={faceCropUrl(m.cover_face_id)} alt="" draggable={false} />
                    <span className="ns-name">{m.name}</span>
                    <span className="ns-count">{m.count.toLocaleString()}</span>
                  </li>
                ))}
                <li className="sb-match sb-new" onClick={() => moveToNewPerson(pickQuery.trim() || undefined)}>
                  + New person{pickQuery.trim() ? ` “${pickQuery.trim()}”` : ""}
                </li>
              </ul>
            </div>
          ) : (
            <>
              <button className="sb-btn" onClick={() => setPicking(true)}>
                Move to…
              </button>
              <button
                className="sb-btn"
                onClick={notThisSelected}
                title="Detach — let each face re-cluster where it belongs"
              >
                Not {name ?? "this person"}
              </button>
              <button className="sb-btn" onClick={ignoreSelected} title="Not a person — hide from People">
                Not a person
              </button>
              <button className="sb-btn ghost" onClick={clearSelection}>
                Cancel
              </button>
            </>
          )}
        </div>
      )}

      {undo && (
        <div className="undo-toast">
          <span>{undo.label}</span>
          <button className="undo-btn" onClick={doUndo}>
            Undo
          </button>
        </div>
      )}

      {viewerIndex !== null && (
        <Lightbox
          index={viewerIndex}
          total={total}
          resolveId={resolveId}
          onClose={() => setViewerIndex(null)}
          onCorrection={reloadPhotos}
        />
      )}
    </div>
  );

  function renderCellContent(photo: PhotoRow) {
    if (readyRef.current.has(photo.id)) {
      return <img src={thumbUrl(photo.id)} className="thumb" loading="eager" decoding="async" draggable={false} />;
    }
    if (photo.status === STATUS_DOWNLOADING) {
      return <div className="cell-overlay" aria-label="downloading"><span className="spinner" /></div>;
    }
    if (photo.status === STATUS_CLOUD) {
      return <div className="cell-overlay" aria-label="in the cloud" />;
    }
    if (photo.status === STATUS_FAILED) {
      return <div className="cell-overlay failed" aria-label="couldn't read" />;
    }
    return null; // pending — gray box
  }
}
