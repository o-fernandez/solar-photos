// The virtualized photo grid.
//
// Why virtualized: at 100k photos we cannot put 100k <img> elements in the DOM
// — the browser would choke. Instead we render only the handful of rows that
// are actually on screen (plus a small overscan), and compute the position of
// everything else as math. This is what keeps scrolling at 60fps regardless of
// library size (Principle 6).
//
// How it honors "never reflow / never lose the user's place" (Principle 2):
//   * Every cell is a fixed-size box. A thumbnail loads *inside* that box, so
//     swapping a gray placeholder for an image never changes layout.
//   * Photos are kept in discovery order, so a live scan only ever *appends*
//     cells to the end — what the user is looking at never shifts.
//   * Thumbnail events update only the cells they affect.
//
// Cloud-only photos show a distinct placeholder; when the user scrolls to them
// the backend fetches them on demand (we report the visible range below), they
// flip to a spinner, then to the image once cached.

import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import Lightbox from "./Lightbox";
import {
  getPhotosRange,
  onThumbDownloading,
  onThumbReady,
  setPhotoFavorite,
  setPhotoHidden,
  setVisibleRange,
  thumbUrl,
  STATUS_READY,
  STATUS_CLOUD,
  STATUS_DOWNLOADING,
  STATUS_FAILED,
  type PhotoFilter,
  type PhotoRow,
} from "./api";

const GAP = 4; // px between cells
const TARGET_CELL = 200; // px — desired cell edge; actual size flexes to fill width
const CHUNK = 200; // how many rows we fetch from the DB per request
const OVERSCAN_ROWS = 4; // rows rendered just outside the viewport, for smoothness

function PhotoGrid({
  total,
  byDate,
  refreshKey,
  filter = "visible",
  onCurationChanged,
}: {
  total: number;
  byDate: boolean;
  refreshKey: number;
  /** Which curation slice this grid shows (timeline / favorites / hidden). */
  filter?: PhotoFilter;
  /** Called after a star/hide toggle so the parent can refresh counts and, when
   *  the toggle changed this view's membership, trigger a scroll-preserving
   *  refetch (via a bumped refreshKey/curation counter). */
  onCurationChanged?: () => void;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  // Local favorite overrides (id → on) so a star fills instantly without a
  // refetch; seeded from row data, authoritative until the next load.
  const favRef = useRef<Map<number, boolean>>(new Map());
  const isFav = (photo: PhotoRow) => favRef.current.get(photo.id) ?? photo.favorite ?? false;

  // --- Photo data, kept in refs so frequent updates don't re-render on their
  // own. A `tick` state, bumped at most once per animation frame, drives the
  // actual re-render so bursts of thumbnail events coalesce into one paint. ---
  const photosRef = useRef<(PhotoRow | undefined)[]>([]);
  const readyRef = useRef<Set<number>>(new Set());
  const downloadingRef = useRef<Set<number>>(new Set());
  const loadedChunks = useRef<Set<number>>(new Set());
  const [, setTick] = useState(0);

  // The viewer: which library index is open (null = closed).
  const [viewerIndex, setViewerIndex] = useState<number | null>(null);

  // Resolve the photo id at an index for the viewer, loading its chunk if the
  // grid hasn't fetched that far yet.
  const resolveId = useCallback(async (i: number): Promise<number | null> => {
    const existing = photosRef.current[i];
    if (existing) return existing.id;
    const c = Math.floor(i / CHUNK);
    try {
      const rows = await getPhotosRange(c * CHUNK, CHUNK, byDate, filter);
      rows.forEach((row, k) => {
        photosRef.current[c * CHUNK + k] = row;
        if (row.status === STATUS_READY) readyRef.current.add(row.id);
      });
      loadedChunks.current.add(c);
      return photosRef.current[i]?.id ?? null;
    } catch {
      return null;
    }
  }, [byDate, filter]);

  const invalidatePending = useRef(false);
  const invalidate = useCallback(() => {
    if (invalidatePending.current) return;
    invalidatePending.current = true;
    requestAnimationFrame(() => {
      invalidatePending.current = false;
      setTick((t) => t + 1);
    });
  }, []);

  // Drop the loaded index→photo map and refetch the visible span — WITHOUT
  // touching scrollTop. Used when a toggle removes a photo from this view: the
  // photos below compact up by one cell, but the pixel offset stays, so the user
  // keeps their place (Principle 2). Distinct from the refreshKey reset, which
  // starts fresh at the top for a whole-library change.
  const softRefetch = useCallback(() => {
    photosRef.current = [];
    loadedChunks.current.clear();
    invalidate();
  }, [invalidate]);

  // Star toggle: fills instantly (favRef override), persists, and — in the
  // Favorites view, where un-starring removes the photo — refetches in place.
  const toggleFavorite = useCallback(
    (photo: PhotoRow) => {
      const next = !(favRef.current.get(photo.id) ?? photo.favorite ?? false);
      favRef.current.set(photo.id, next);
      invalidate();
      setPhotoFavorite(photo.id, next).catch(() => {});
      if (filter === "favorites" && !next) softRefetch();
      onCurationChanged?.();
    },
    [filter, invalidate, softRefetch, onCurationChanged],
  );

  // Hide (timeline/favorites) or restore (hidden view): the photo leaves this
  // view, so refetch in place. Files are never touched — just the flag. A grid
  // hide is instant and easy to fat-finger, so it always offers an Undo.
  const undoTimer = useRef<number | undefined>(undefined);
  const setHidden = useCallback(
    (photo: PhotoRow, hidden: boolean) => {
      setPhotoHidden(photo.id, hidden).catch(() => {});
      softRefetch();
      onCurationChanged?.();
      if (undoTimer.current) window.clearTimeout(undoTimer.current);
      setHideUndo({ id: photo.id, hidden });
      undoTimer.current = window.setTimeout(() => setHideUndo(null), 6000);
    },
    [softRefetch, onCurationChanged],
  );
  // The last hide/restore, revertable for a few seconds.
  const [hideUndo, setHideUndo] = useState<{ id: number; hidden: boolean } | null>(null);
  const undoHide = useCallback(() => {
    setHideUndo((u) => {
      if (u) {
        setPhotoHidden(u.id, !u.hidden).catch(() => {});
        softRefetch();
        onCurationChanged?.();
      }
      return null;
    });
  }, [softRefetch, onCurationChanged]);

  // As the library grows during a live scan, the chunk that held the previous
  // last photo may have been loaded only partially — evict it so it refetches.
  // New chunks load on demand as the user scrolls into them.
  const prevTotal = useRef(0);
  useEffect(() => {
    if (total > prevTotal.current && prevTotal.current > 0) {
      loadedChunks.current.delete(Math.floor((prevTotal.current - 1) / CHUNK));
    }
    prevTotal.current = total;
    invalidate();
  }, [total, invalidate]);

  // Reset when the ordering changes (scan finished: discovery → date) or the
  // library changed on disk (add / rescan / remove → refreshKey). The
  // index→photo mapping is order-dependent, so drop it and refetch from the top;
  // readyRef/downloadingRef are id-based and stay valid. The discovery→date flip
  // reorders everything under the user, so it announces itself briefly.
  const firstOrder = useRef(true);
  const prevByDate = useRef(byDate);
  const [sortNote, setSortNote] = useState(false);
  useEffect(() => {
    const orderFlipped = !prevByDate.current && byDate;
    prevByDate.current = byDate;
    if (firstOrder.current) {
      firstOrder.current = false;
      return;
    }
    photosRef.current = [];
    loadedChunks.current.clear();
    if (scrollRef.current) scrollRef.current.scrollTop = 0;
    invalidate();
    // Only the discovery→date flip reorders under the user; refreshKey bumps
    // (add / rescan / remove) keep the same ordering and stay silent.
    if (orderFlipped) {
      setSortNote(true);
      const t = window.setTimeout(() => setSortNote(false), 5000);
      return () => window.clearTimeout(t);
    }
  }, [byDate, refreshKey, invalidate]);

  // --- Timeline scrubber ---
  const [scrubbing, setScrubbing] = useState(false);
  // Mirrors draggingRef for render: while actually dragging the scrubber its own
  // label shows; during ordinary scrolling the floating month chip does instead.
  const [dragging, setDragging] = useState(false);
  const trackRef = useRef<HTMLDivElement>(null);
  const draggingRef = useRef(false);
  const hideTimer = useRef<number | undefined>(undefined);

  const bumpHide = useCallback(() => {
    if (hideTimer.current) window.clearTimeout(hideTimer.current);
    hideTimer.current = window.setTimeout(() => setScrubbing(false), 900);
  }, []);

  const onScroll = useCallback(() => {
    setScrubbing(true);
    bumpHide();
  }, [bumpHide]);

  const scrubTo = useCallback((clientY: number) => {
    const track = trackRef.current;
    const el = scrollRef.current;
    if (!track || !el) return;
    const rect = track.getBoundingClientRect();
    const frac = Math.min(1, Math.max(0, (clientY - rect.top) / rect.height));
    el.scrollTop = frac * Math.max(1, el.scrollHeight - el.clientHeight);
  }, []);

  const onScrubPointerDown = useCallback(
    (e: React.PointerEvent) => {
      draggingRef.current = true;
      setDragging(true);
      setScrubbing(true);
      scrubTo(e.clientY);
      const move = (ev: PointerEvent) => draggingRef.current && scrubTo(ev.clientY);
      const up = () => {
        draggingRef.current = false;
        setDragging(false);
        window.removeEventListener("pointermove", move);
        window.removeEventListener("pointerup", up);
        bumpHide();
      };
      window.addEventListener("pointermove", move);
      window.addEventListener("pointerup", up);
    },
    [scrubTo, bumpHide],
  );

  // Thumbnail-ready pushes: record the id, drop any downloading marker, repaint.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onThumbReady((d) => {
      // Only show an image when one actually exists. A failed/abandoned cloud
      // download just stops the spinner; the cell falls back to its cloud glyph.
      if (d.ok) readyRef.current.add(d.id);
      downloadingRef.current.delete(d.id);
      invalidate();
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, [invalidate]);

  // "Started downloading these cloud photos" — show a spinner on those cells.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onThumbDownloading((ids) => {
      ids.forEach((id) => downloadingRef.current.add(id));
      invalidate();
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
    const ro = new ResizeObserver((entries) => {
      setWidth(entries[0].contentRect.width);
    });
    ro.observe(el);
    setWidth(el.clientWidth);
    return () => ro.disconnect();
  }, []);

  const { columns, cellSize, rowHeight, rowCount } = useMemo(() => {
    const w = Math.max(width, TARGET_CELL);
    const cols = Math.max(1, Math.floor((w + GAP) / (TARGET_CELL + GAP)));
    const size = Math.floor((w - GAP * (cols - 1)) / cols);
    return {
      columns: cols,
      cellSize: size,
      rowHeight: size + GAP,
      rowCount: Math.ceil(total / cols),
    };
  }, [width, total]);

  const rowVirtualizer = useVirtualizer({
    count: rowCount,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => rowHeight,
    overscan: OVERSCAN_ROWS,
  });

  const virtualRows = rowVirtualizer.getVirtualItems();

  // --- Lazily load the photo rows we're about to render, and tell the backend
  // which photos are visible so their thumbnails get priority / cloud photos
  // get fetched on demand (Principle 3). ---
  const prioritizeTimer = useRef<number | undefined>(undefined);

  useEffect(() => {
    if (virtualRows.length === 0 || columns === 0) return;

    const firstRow = virtualRows[0].index;
    const lastRow = virtualRows[virtualRows.length - 1].index;
    const firstIndex = firstRow * columns;
    const lastIndex = Math.min(total - 1, (lastRow + 1) * columns - 1);

    // Fetch any unloaded chunks covering the visible span.
    const firstChunk = Math.floor(firstIndex / CHUNK);
    const lastChunk = Math.floor(lastIndex / CHUNK);
    for (let c = firstChunk; c <= lastChunk; c++) {
      if (loadedChunks.current.has(c)) continue;
      loadedChunks.current.add(c);
      getPhotosRange(c * CHUNK, CHUNK, byDate, filter)
        .then((rows) => {
          rows.forEach((row, i) => {
            photosRef.current[c * CHUNK + i] = row;
            if (row.status === STATUS_READY) readyRef.current.add(row.id);
          });
          invalidate();
        })
        .catch(() => {
          loadedChunks.current.delete(c); // allow a retry later
        });
    }

    // Debounced: report the visible (loaded, not-yet-ready) photo ids. The
    // backend prioritizes local thumbnails and fetches cloud photos on demand.
    if (prioritizeTimer.current) window.clearTimeout(prioritizeTimer.current);
    prioritizeTimer.current = window.setTimeout(() => {
      const ids: number[] = [];
      for (let i = firstIndex; i <= lastIndex; i++) {
        const p = photosRef.current[i];
        if (p && !readyRef.current.has(p.id)) ids.push(p.id);
      }
      setVisibleRange(ids).catch(() => {});
    }, 80);
  }, [virtualRows, columns, total, byDate, filter, invalidate]);

  // Scrubber geometry, read fresh each render (the virtualizer re-renders on
  // scroll, so this stays in sync without a separate scroll-position state).
  const totalSize = rowVirtualizer.getTotalSize();
  const viewport = scrollRef.current?.clientHeight ?? 0;
  const scrollTop = scrollRef.current?.scrollTop ?? 0;
  const maxScroll = Math.max(1, totalSize - viewport);
  const frac = Math.min(1, Math.max(0, scrollTop / maxScroll));
  const thumbH = viewport > 0 ? Math.max(36, viewport * (viewport / Math.max(totalSize, viewport))) : 36;
  const thumbTop = frac * (viewport - thumbH);
  const topIndex = rowHeight > 0 ? Math.floor(scrollTop / rowHeight) * columns : 0;
  const topTs = photosRef.current[topIndex]?.ts;
  const showScrubber = totalSize > viewport + 1 && viewport > 0;

  return (
    <>
    <div className="grid-wrap">
    <div ref={scrollRef} className="grid-scroll" onScroll={onScroll}>
      <div
        style={{
          height: `${rowVirtualizer.getTotalSize()}px`,
          width: "100%",
          position: "relative",
        }}
      >
        {virtualRows.map((virtualRow) => {
          const rowStart = virtualRow.index * columns;
          const cells = [];
          for (let c = 0; c < columns; c++) {
            const index = rowStart + c;
            if (index >= total) break;
            const photo = photosRef.current[index];
            const fav = photo ? isFav(photo) : false;
            cells.push(
              <div
                key={index}
                className={`cell${fav ? " is-fav" : ""}`}
                role="button"
                tabIndex={-1}
                onClick={() => setViewerIndex(index)}
                style={{
                  width: cellSize,
                  height: cellSize,
                  marginRight: c < columns - 1 ? GAP : 0,
                  cursor: "pointer",
                }}
              >
                {renderCellContent(photo)}
                {photo && (
                  <div className="cell-actions">
                    <button
                      className={`cell-fav${fav ? " on" : ""}`}
                      title={fav ? "Remove favorite" : "Favorite"}
                      aria-label={fav ? "Remove favorite" : "Favorite"}
                      aria-pressed={fav}
                      onClick={(e) => {
                        e.stopPropagation();
                        toggleFavorite(photo);
                      }}
                    >
                      <HeartGlyph filled={fav} />
                    </button>
                    <button
                      className="cell-hide"
                      title={filter === "hidden" ? "Restore to timeline" : "Hide from timeline"}
                      aria-label={filter === "hidden" ? "Restore to timeline" : "Hide from timeline"}
                      onClick={(e) => {
                        e.stopPropagation();
                        setHidden(photo, filter !== "hidden");
                      }}
                    >
                      {filter === "hidden" ? <EyeGlyph /> : <EyeOffGlyph />}
                    </button>
                  </div>
                )}
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
    {/* Date wayfinding while scrolling: a floating month chip fades in at the
        top-left (no layout change, nothing reflows) and out when you stop. While
        dragging the scrubber, its own label next to the thumb takes over. */}
    {byDate && scrubbing && !dragging && topTs ? (
      <div className="month-chip">{monthYear(topTs)}</div>
    ) : null}
    {sortNote && <div className="month-chip sort-note">Scan finished — sorted by date, newest first</div>}
    {showScrubber && (
      <div
        className={`scrubber${scrubbing ? " active" : ""}`}
        ref={trackRef}
        onPointerDown={onScrubPointerDown}
      >
        <div className="scrubber-thumb" style={{ height: thumbH, transform: `translateY(${thumbTop}px)` }} />
        {dragging && topTs ? (
          <div className="scrubber-label" style={{ top: thumbTop + thumbH / 2 }}>
            {monthYear(topTs)}
          </div>
        ) : null}
      </div>
    )}
    {hideUndo && (
      <div className="undo-toast">
        <span>
          {hideUndo.hidden ? "Hidden from timeline" : "Restored to timeline"}
        </span>
        <button className="undo-btn" onClick={undoHide}>
          Undo
        </button>
      </div>
    )}
    </div>
    {viewerIndex !== null && (
      <Lightbox
        index={viewerIndex}
        total={total}
        resolveId={resolveId}
        onClose={() => setViewerIndex(null)}
        onCorrection={() => {
          // A favorite/hide from the viewer: re-pull so the grid underneath
          // reflects it (and counts update), keeping the user's scroll place.
          softRefetch();
          onCurationChanged?.();
        }}
      />
    )}
    </>
  );

  // Decide what a cell shows based on its photo's state. Inner closure so it can
  // read the refs without prop-drilling; cheap, runs only for visible cells.
  function renderCellContent(photo: PhotoRow | undefined) {
    if (!photo) return null; // not loaded yet — gray box
    const id = photo.id;
    if (readyRef.current.has(id)) {
      return (
        <img
          src={thumbUrl(id)}
          className="thumb"
          loading="eager"
          decoding="async"
          draggable={false}
        />
      );
    }
    if (downloadingRef.current.has(id) || photo.status === STATUS_DOWNLOADING) {
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
        >
          <CloudGlyph />
        </div>
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
    return null; // local pending — gray box
  }
}

// Memoized: App re-renders on background-progress ticks; the grid coalesces its
// own updates internally (rAF invalidate) and only needs re-rendering when the
// library itself changes shape.
export default memo(PhotoGrid);

function monthYear(ts: number): string {
  return new Date(ts * 1000).toLocaleDateString(undefined, { month: "short", year: "numeric" });
}

function CloudGlyph() {
  return (
    <svg viewBox="0 0 24 24" width="28" height="28" className="cloud-glyph" aria-hidden="true">
      <path
        fill="currentColor"
        d="M19 18H6a4 4 0 0 1-.5-7.97A5.5 5.5 0 0 1 16.9 9.2 3.5 3.5 0 0 1 19 18Z"
      />
    </svg>
  );
}

function HeartGlyph({ filled }: { filled: boolean }) {
  return (
    <svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true"
      fill={filled ? "currentColor" : "none"} stroke="currentColor" strokeWidth="1.8"
      strokeLinecap="round" strokeLinejoin="round">
      <path d="M12 20.3l-1.45-1.32C5.4 14.24 2 11.16 2 7.5 2 4.42 4.42 2 7.5 2c1.74 0 3.41.81 4.5 2.09C13.09 2.81 14.76 2 16.5 2 19.58 2 22 4.42 22 7.5c0 3.66-3.4 6.74-8.55 11.49L12 20.3z" />
    </svg>
  );
}

function EyeOffGlyph() {
  return (
    <svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true"
      fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
      <path d="M9.9 4.24A9.1 9.1 0 0 1 12 4c7 0 10 8 10 8a18 18 0 0 1-2.16 3.19M6.6 6.6A18 18 0 0 0 2 12s3 8 10 8a9 9 0 0 0 5.4-1.6" />
      <path d="M1 1l22 22" />
    </svg>
  );
}

function EyeGlyph() {
  return (
    <svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true"
      fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
      <path d="M2 12s3-8 10-8 10 8 10 8-3 8-10 8-10-8-10-8z" />
      <circle cx="12" cy="12" r="3" />
    </svg>
  );
}
