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
//   * Thumbnail-ready events update only the cells they affect; results for
//     off-screen photos are recorded silently and shown if/when scrolled to.
//   * We never programmatically scroll. The user's position is theirs alone.

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  getPhotosRange,
  onThumbReady,
  setVisibleRange,
  thumbUrl,
  STATUS_READY,
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
  const loadedChunks = useRef<Set<number>>(new Set());
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

  // Reset all caches when the library size changes (new scan / first load).
  useEffect(() => {
    photosRef.current = new Array(total);
    readyRef.current = new Set();
    loadedChunks.current = new Set();
    invalidate();
  }, [total, invalidate]);

  // Listen for "thumbnail ready" pushes from the backend. We only record the id
  // and schedule a coalesced repaint — no layout work here.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onThumbReady((id) => {
      readyRef.current.add(id);
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
  // which photos are visible so their thumbnails get priority (Principle 3). ---
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

    // Debounced: report the visible (loaded, not-yet-ready) photo ids so the
    // backend bumps them to the front of the work queue.
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
            const ready = photo ? readyRef.current.has(photo.id) : false;
            cells.push(
              <div
                key={index}
                className="cell"
                style={{ width: cellSize, height: cellSize, marginRight: c < columns - 1 ? GAP : 0 }}
              >
                {photo && ready ? (
                  <img
                    src={thumbUrl(photo.id)}
                    className="thumb"
                    loading="eager"
                    decoding="async"
                    draggable={false}
                  />
                ) : null}
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
  );
}
