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

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import Lightbox from "./Lightbox";
import {
  getPhotosRange,
  onThumbDownloading,
  onThumbReady,
  setVisibleRange,
  thumbUrl,
  STATUS_READY,
  STATUS_CLOUD,
  STATUS_DOWNLOADING,
  STATUS_FAILED,
  type PhotoRow,
} from "./api";

const GAP = 4; // px between cells
const TARGET_CELL = 200; // px — desired cell edge; actual size flexes to fill width
const CHUNK = 200; // how many rows we fetch from the DB per request
const OVERSCAN_ROWS = 4; // rows rendered just outside the viewport, for smoothness

export default function PhotoGrid({ total }: { total: number }) {
  const scrollRef = useRef<HTMLDivElement>(null);

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
      const rows = await getPhotosRange(c * CHUNK, CHUNK);
      rows.forEach((row, k) => {
        photosRef.current[c * CHUNK + k] = row;
        if (row.status === STATUS_READY) readyRef.current.add(row.id);
      });
      loadedChunks.current.add(c);
      return photosRef.current[i]?.id ?? null;
    } catch {
      return null;
    }
  }, []);

  const invalidatePending = useRef(false);
  const invalidate = useCallback(() => {
    if (invalidatePending.current) return;
    invalidatePending.current = true;
    requestAnimationFrame(() => {
      invalidatePending.current = false;
      setTick((t) => t + 1);
    });
  }, []);

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

  // Thumbnail-ready pushes: record the id, drop any downloading marker, repaint.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onThumbReady((id) => {
      readyRef.current.add(id);
      downloadingRef.current.delete(id);
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
      getPhotosRange(c * CHUNK, CHUNK)
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
  }, [virtualRows, columns, total, invalidate]);

  return (
    <>
    <div ref={scrollRef} className="grid-scroll">
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
            cells.push(
              <div
                key={index}
                className="cell"
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
    {viewerIndex !== null && (
      <Lightbox
        index={viewerIndex}
        total={total}
        resolveId={resolveId}
        onClose={() => setViewerIndex(null)}
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
      return <div className="cell-overlay" aria-label="downloading"><span className="spinner" /></div>;
    }
    if (photo.status === STATUS_CLOUD) {
      return <div className="cell-overlay" aria-label="in the cloud"><CloudGlyph /></div>;
    }
    if (photo.status === STATUS_FAILED) {
      return <div className="cell-overlay failed" aria-label="couldn't read" />;
    }
    return null; // local pending — gray box
  }
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
