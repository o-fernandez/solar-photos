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
import FacePeek from "./FacePeek";
import {
  absorbClusters,
  detachFaces,
  faceCropUrl,
  faceIdsForPhotos,
  getClusterGeneration,
  getClusters,
  getPersonLooks,
  getPersonPhotos,
  ignoreFaces,
  mergeClusters,
  nameCluster,
  notThisPerson,
  notThisPersonMany,
  onClusterProgress,
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
import { usePickerNav } from "./pickerNav";

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
// `unresolve` re-shows review-band chips a bulk answer hid.
interface PendingUndo {
  rows: PhotoRow[];
  undo: CorrectionUndo;
  label: string;
  unresolve?: number[];
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
  // The person's current group key. Naming a fresh (positive, appearance) group
  // promotes it to a durable identity under a NEW negative key; the backend
  // returns the canonical key so the open page keeps following the same person.
  const [groupId, setGroupId] = useState(cluster.cluster_id);
  const [viewerIndex, setViewerIndex] = useState<number | null>(null);
  const [undo, setUndo] = useState<PendingUndo | null>(null);
  // A transient message (no Undo) — e.g. a correction refused because a background
  // re-cluster renumbered ids since the page loaded; the user just retries.
  const [notice, setNotice] = useState<string | null>(null);
  const noticeTimer = useRef<number | undefined>(undefined);
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
    getPersonPhotos(groupId)
      .then((r) => {
        r.forEach((row) => {
          if (row.status === STATUS_READY) readyRef.current.add(row.id);
        });
        setRows(r);
        setLoaded(true);
      })
      .catch(() => setLoaded(true));
  }, [groupId]);

  // Load (and reload) this person's "looks" — the appearance sub-clusters. Refreshed
  // after any correction, since moving faces changes the grouping (and clears a flag).
  const loadLooks = useCallback(() => {
    getPersonLooks(groupId)
      .then((l) => {
        setLooks(l);
        setSelectedLook((cur) => (cur != null && cur >= l.length ? null : cur));
      })
      .catch(() => {});
  }, [groupId]);

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
      .filter((c) => c.cluster_id !== groupId && c.name && c.name.toLowerCase().includes(s))
      .slice(0, 5);
  };
  const exactNameMatch = (q: string): Cluster | undefined => {
    const s = q.trim().toLowerCase();
    if (!s) return undefined;
    return people.find(
      (c) => c.cluster_id !== groupId && c.name != null && c.name.toLowerCase() === s,
    );
  };
  // Fold this whole person into another (picked, or typed as an exact match), then
  // leave the page — this cluster is now part of the other.
  const mergeThisInto = (target: Cluster) => {
    setEditing(false);
    mergeClusters(target.cluster_id, groupId, genRef.current)
      .then(onBack)
      .catch(() => flashNotice("People were just reorganized — try that again."));
  };

  const commitName = () => {
    const value = draft.trim();
    setEditing(false);
    const match = value ? exactNameMatch(value) : undefined;
    if (match) {
      mergeThisInto(match);
      return;
    }
    nameCluster(groupId, value, genRef.current)
      .then((g) => {
        setGroupId(g);
        setName(value || null);
      })
      .catch(() => flashNotice("People were just reorganized — try that again."));
  };

  // A review-band face being peeked at full-photo size (a crop alone often isn't
  // enough to say who someone is).
  const [peekFace, setPeekFace] = useState<number | null>(null);

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

  // Bulk answer for the whole band — a dozen 1-photo chips is glanceable in one
  // look, and answering them one ✓ at a time read as manual labor. One action,
  // one undo (which also re-shows the chips).
  const resolveReviewAll = (keep: boolean) => {
    if (!review || reviewLeft.length === 0) return;
    const ids = reviewLeft.map((c) => c.cluster_id);
    const who = name ?? review.name;
    setReviewResolved((s) => new Set([...s, ...ids]));
    (keep
      ? absorbClusters(review.into, ids, review.generation)
      : notThisPersonMany(review.into, ids, review.generation)
    )
      .then((tok) => {
        if (keep) reloadPhotos();
        setUndo({
          rows: [],
          undo: tok,
          label: keep
            ? `Added ${ids.length} groups to ${who}`
            : `${ids.length} groups marked not ${who}`,
          unresolve: ids,
        });
        if (undoTimer.current) window.clearTimeout(undoTimer.current);
        undoTimer.current = window.setTimeout(() => setUndo(null), 8000);
      })
      .catch(() => {
        setReviewResolved((s) => {
          const next = new Set(s);
          ids.forEach((id) => next.delete(id));
          return next;
        });
        flashNotice("People were just reorganized — try that again.");
      });
  };

  // The people you can reassign a chunk *into* — every other person, biggest first —
  // plus the clustering generation their ids belong to. Both refresh when a
  // background re-cluster finishes (it renumbers cluster ids), so a move started
  // after that binds against current ids; a move racing the boundary is refused by
  // the backend's generation check instead of landing on the wrong person.
  const genRef = useRef<number>(0);
  useEffect(() => {
    const refreshIds = () => {
      getClusters().then(setPeople).catch(() => {});
      getClusterGeneration()
        .then((g) => {
          genRef.current = g;
        })
        .catch(() => {});
    };
    refreshIds();
    let unlisten: (() => void) | undefined;
    onClusterProgress((p) => {
      if (!p.running) {
        // A background re-cluster finished — it renumbered cluster ids and may have
        // moved faces in or out of this person. Refresh the id/generation AND the
        // shown photos + looks, so the page never sits on a stale grouping (which
        // left already-moved looks lingering until the next reload).
        refreshIds();
        reloadPhotos();
        loadLooks();
      }
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, [reloadPhotos, loadLooks]);

  const toggleSelect = (photoId: number) => {
    setSelected((s) => {
      const next = new Set(s);
      next.has(photoId) ? next.delete(photoId) : next.add(photoId);
      return next;
    });
  };
  // Shift-click selects the whole range from the last-toggled cell — selecting a
  // 60-photo chunk one click at a time doesn't survive a 4,000-photo person (P6).
  const anchorIndexRef = useRef<number | null>(null);
  const handleCellSelect = (index: number, shiftRange: boolean) => {
    if (shiftRange && anchorIndexRef.current != null) {
      const lo = Math.min(anchorIndexRef.current, index);
      const hi = Math.max(anchorIndexRef.current, index);
      setSelected((s) => {
        const next = new Set(s);
        for (let i = lo; i <= hi; i++) {
          const r = rowsRef.current[i];
          if (r) next.add(r.id);
        }
        return next;
      });
    } else {
      const r = rowsRef.current[index];
      if (r) toggleSelect(r.id);
    }
    anchorIndexRef.current = index;
  };
  const clearSelection = () => {
    setSelected(new Set());
    setPicking(false);
    setPickQuery("");
    anchorIndexRef.current = null;
  };

  // Flash a transient, button-less message (distinct from the Undo toast).
  const flashNotice = useCallback((msg: string) => {
    setNotice(msg);
    if (noticeTimer.current) window.clearTimeout(noticeTimer.current);
    noticeTimer.current = window.setTimeout(() => setNotice(null), 4500);
  }, []);

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
        const faceIds = await faceIdsForPhotos(photoIds, groupId);
        const tok = await run(faceIds);
        setUndo({ rows: removed, undo: tok, label });
        if (undoTimer.current) window.clearTimeout(undoTimer.current);
        undoTimer.current = window.setTimeout(() => setUndo(null), 6000);
        // Moving faces out changes the look grouping (and clears a repair flag).
        loadLooks();
      } catch (e) {
        // The move was refused — almost always a stale generation: a background
        // re-cluster renumbered cluster ids since this page loaded, so the backend
        // rejects binding faces to a now-wrong id (see ensure_generation). Failing
        // silently made a refused "Move to X" look like a no-op (the look stayed).
        // Restore the cells, refresh ids + looks so the *next* attempt binds against
        // current ids, and say so.
        setRows((rs) => removed.reduce((acc, r) => insertSorted(acc, r), rs));
        getClusterGeneration().then((g) => { genRef.current = g; }).catch(() => {});
        getClusters().then(setPeople).catch(() => {});
        reloadPhotos();
        loadLooks();
        const msg = String(e ?? "");
        flashNotice(
          /stale|reorganiz/i.test(msg)
            ? "People were just reorganized — try that again."
            : "Couldn't apply that — try again.",
        );
      }
    },
    [groupId, loadLooks, reloadPhotos, flashNotice],
  );

  const doUndo = () => {
    if (!undo) return;
    const { rows: removed, undo: tok, unresolve } = undo;
    setUndo(null);
    undoCorrection(tok)
      .then(() => {
        setRows((rs) => removed.reduce((acc, r) => insertSorted(acc, r), rs));
        if (unresolve) {
          setReviewResolved((s) => {
            const next = new Set(s);
            unresolve.forEach((id) => next.delete(id));
            return next;
          });
          // A bulk "all are them" pulled photos in; refetch settles the grid.
          reloadPhotos();
        }
        loadLooks();
      })
      .catch(() => {});
  };

  const selectedIds = useMemo(() => [...selected], [selected]);
  const moveToPerson = (target: Cluster) =>
    applyCorrection(
      selectedIds,
      (fids) => reassignFacesToCluster(fids, groupId, target.cluster_id, genRef.current),
      `Moved to ${target.name}`,
    );
  const moveToNewPerson = (newName?: string) =>
    applyCorrection(
      selectedIds,
      (fids) => reassignFacesToNewPerson(fids, groupId, newName, genRef.current),
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
    applyCorrection(
      ids,
      (fids) => reassignFacesToCluster(fids, groupId, targetCluster, genRef.current),
      label,
    );
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
      (fids) => reassignFacesToNewPerson(fids, groupId, newName, genRef.current),
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
    rejectMerge(groupId, activeLook.likely_other_cluster)
      .then(loadLooks)
      .catch(() => flashNotice("People were just reorganized — try that again."));
  };

  // Named people other than the one we're viewing, filtered by a typeahead — shared by
  // the multi-select move picker and the per-look move picker.
  const filterPeople = (q: string) =>
    people
      .filter((c) => c.cluster_id !== groupId && c.name)
      .filter((c) => (q.trim() ? c.name!.toLowerCase().includes(q.trim().toLowerCase()) : true))
      .slice(0, 6);
  const pickMatches = useMemo(() => filterPeople(pickQuery), [people, pickQuery, groupId]); // eslint-disable-line react-hooks/exhaustive-deps
  const lookPickMatches = useMemo(() => filterPeople(lookPickQuery), [people, lookPickQuery, groupId]); // eslint-disable-line react-hooks/exhaustive-deps

  // ↑/↓ + Enter in the pickers: Enter takes the highlighted row — the top match by
  // default, so a half-typed name never silently mints a duplicate person. The
  // "+ New person" row is the explicit last row.
  const pickNav = usePickerNav(pickMatches.length + 1, (i) => {
    if (i < pickMatches.length) moveToPerson(pickMatches[i]);
    else moveToNewPerson(pickQuery.trim() || undefined);
  });
  const lookNav = usePickerNav(lookPickMatches.length + 1, (i) => {
    if (i < lookPickMatches.length) moveLookToPerson(lookPickMatches[i]);
    else moveLookToNewPerson(lookPickQuery.trim() || undefined);
  });
  // The rename combobox starts un-highlighted: Enter commits the typed name;
  // arrows opt into the merge suggestions.
  const renameMatches = editing ? nameMatches(draft) : [];
  const renameNav = usePickerNav(renameMatches.length, (i) => mergeThisInto(renameMatches[i]), {
    startUnselected: true,
  });

  // Esc walks back the transient layers: picker → selection → look. Ignored while
  // the Lightbox is open (it owns Esc) or while typing in a field.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || viewerIndex !== null) return;
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA")) return;
      if (picking) setPicking(false);
      else if (selected.size > 0) clearSelection();
      else if (lookPicking) setLookPicking(false);
      else if (selectedLook != null) {
        setSelectedLook(null);
        setLookPicking(false);
        setLookPickQuery("");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [viewerIndex, picking, selected, lookPicking, selectedLook]);

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
              onChange={(e) => {
                setDraft(e.target.value);
                renameNav.resetHighlight();
              }}
              onKeyDown={(e) => {
                if (e.key === "Escape") setEditing(false);
                else if (renameNav.onNavKey(e)) return;
                else if (e.key === "Enter") commitName();
              }}
              onBlur={commitName}
            />
            {renameMatches.length > 0 && (
              // preventDefault keeps the input from blurring (and rename-committing)
              // before a suggestion click runs its merge.
              <ul className="name-suggest" onMouseDown={(e) => e.preventDefault()}>
                <li className="name-suggest-head">Add to an existing person</li>
                {renameMatches.map((m, i) => (
                  <li
                    key={m.cluster_id}
                    className={`name-suggest-item${renameNav.highlight === i ? " hi" : ""}`}
                    onMouseEnter={() => renameNav.setHighlight(i)}
                    onClick={() => mergeThisInto(m)}
                  >
                    <img className="ns-face" src={faceCropUrl(m.cover_face_id)} alt="" draggable={false} />
                    <span className="ns-name">{m.name}</span>
                    <span className="ns-count">{m.count.toLocaleString()}</span>
                  </li>
                ))}
              </ul>
            )}
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
        <div className="person-looks-wrap">
          <div className="looks-head">
            Looks
            <span className="looks-hint"> — tap one to filter their photos, or to move a batch</span>
          </div>
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
                onChange={(e) => {
                  setLookPickQuery(e.target.value);
                  lookNav.resetHighlight();
                }}
                onKeyDown={(e) => {
                  if (e.key === "Escape") setLookPicking(false);
                  else lookNav.onNavKey(e);
                }}
              />
              <ul className="sb-matches">
                {lookPickMatches.map((m, i) => (
                  <li
                    key={m.cluster_id}
                    className={`sb-match${lookNav.highlight === i ? " hi" : ""}`}
                    onMouseEnter={() => lookNav.setHighlight(i)}
                    onClick={() => moveLookToPerson(m)}
                  >
                    <img className="ns-face" src={faceCropUrl(m.cover_face_id)} alt="" draggable={false} />
                    <span className="ns-name">{m.name}</span>
                    <span className="ns-count">{m.count.toLocaleString()}</span>
                  </li>
                ))}
                <li
                  className={`sb-match sb-new${lookNav.highlight === lookPickMatches.length ? " hi" : ""}`}
                  onMouseEnter={() => lookNav.setHighlight(lookPickMatches.length)}
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
            <span>
              {reviewLeft.length.toLocaleString()} {reviewLeft.length === 1 ? "group" : "groups"}{" "}
              might also be <b>{name ?? review!.name}</b> — check each
            </span>
            {reviewLeft.length > 1 && (
              <span className="pr-bulk">
                <button className="pr-bulk-btn" onClick={() => resolveReviewAll(true)}>
                  All are {name ?? review!.name}
                </button>
                <button className="pr-bulk-btn no" onClick={() => resolveReviewAll(false)}>
                  None are {name ?? review!.name}
                </button>
              </span>
            )}
          </div>
          <div className="pr-row">
            {reviewLeft.map((c) => (
              <div className="pr-chip" key={c.cluster_id}>
                {c.face_id != null ? (
                  <img
                    className="pr-face peekable"
                    src={faceCropUrl(c.face_id)}
                    alt=""
                    draggable={false}
                    title="See the full photo"
                    onClick={() => setPeekFace(c.face_id!)}
                  />
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
                      // Once a selection is underway, taps add/remove from it (shift
                      // extends the range); otherwise a tap opens the photo. The
                      // checkbox always toggles selection.
                      onClick={(e) =>
                        selecting ? handleCellSelect(index, e.shiftKey) : setViewerIndex(index)
                      }
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
                          handleCellSelect(index, e.shiftKey);
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
                onChange={(e) => {
                  setPickQuery(e.target.value);
                  pickNav.resetHighlight();
                }}
                onKeyDown={(e) => {
                  if (e.key === "Escape") setPicking(false);
                  else pickNav.onNavKey(e);
                }}
              />
              <ul className="sb-matches">
                {pickMatches.map((m, i) => (
                  <li
                    key={m.cluster_id}
                    className={`sb-match${pickNav.highlight === i ? " hi" : ""}`}
                    onMouseEnter={() => pickNav.setHighlight(i)}
                    onClick={() => moveToPerson(m)}
                  >
                    <img className="ns-face" src={faceCropUrl(m.cover_face_id)} alt="" draggable={false} />
                    <span className="ns-name">{m.name}</span>
                    <span className="ns-count">{m.count.toLocaleString()}</span>
                  </li>
                ))}
                <li
                  className={`sb-match sb-new${pickNav.highlight === pickMatches.length ? " hi" : ""}`}
                  onMouseEnter={() => pickNav.setHighlight(pickMatches.length)}
                  onClick={() => moveToNewPerson(pickQuery.trim() || undefined)}
                >
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

      {notice && !undo && (
        <div className="undo-toast">
          <span>{notice}</span>
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

      {peekFace != null && <FacePeek faceId={peekFace} onClose={() => setPeekFace(null)} />}
    </div>
  );

  function renderCellContent(photo: PhotoRow) {
    if (readyRef.current.has(photo.id)) {
      return <img src={thumbUrl(photo.id)} className="thumb" loading="eager" decoding="async" draggable={false} />;
    }
    if (photo.status === STATUS_DOWNLOADING) {
      return (
        <div className="cell-overlay" aria-label="downloading" title="Downloading…">
          <span className="spinner" />
        </div>
      );
    }
    if (photo.status === STATUS_CLOUD) {
      return (
        <div
          className="cell-overlay"
          aria-label="in the cloud"
          title="In the cloud — downloads when you view it"
        />
      );
    }
    if (photo.status === STATUS_FAILED) {
      return (
        <div
          className="cell-overlay failed"
          aria-label="couldn't read"
          title="Couldn't read this photo"
        />
      );
    }
    return null; // pending — gray box
  }
}
