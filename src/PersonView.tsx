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
  faceCropUrl,
  getPersonPhotos,
  nameCluster,
  onThumbReady,
  removePersonFace,
  restorePersonFaces,
  setVisibleRange,
  thumbUrl,
  STATUS_READY,
  STATUS_DOWNLOADING,
  STATUS_CLOUD,
  STATUS_FAILED,
  type Cluster,
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

interface PendingUndo {
  row: PhotoRow;
  faceIds: number[];
}

export default function PersonView({
  cluster,
  onBack,
}: {
  cluster: Cluster;
  onBack: () => void;
}) {
  const [rows, setRows] = useState<PhotoRow[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");
  const [name, setName] = useState(cluster.name);
  const [viewerIndex, setViewerIndex] = useState<number | null>(null);
  const [undo, setUndo] = useState<PendingUndo | null>(null);

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

  // resolveId reads the latest rows without re-subscribing the viewer.
  const rowsRef = useRef<PhotoRow[]>(rows);
  rowsRef.current = rows;
  const resolveId = useCallback(
    (i: number): Promise<number | null> => Promise.resolve(rowsRef.current[i]?.id ?? null),
    [],
  );

  // Load this person's photos once on mount (the whole set is known — no paging).
  useEffect(() => {
    let alive = true;
    getPersonPhotos(cluster.cluster_id)
      .then((r) => {
        if (!alive) return;
        r.forEach((row) => {
          if (row.status === STATUS_READY) readyRef.current.add(row.id);
        });
        setRows(r);
        setLoaded(true);
      })
      .catch(() => alive && setLoaded(true));
    return () => {
      alive = false;
    };
  }, [cluster.cluster_id]);

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

  const total = rows.length;
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
        const p = rows[i];
        if (p && !readyRef.current.has(p.id)) ids.push(p.id);
      }
      setVisibleRange(ids).catch(() => {});
    }, 80);
  }, [virtualRows, columns, total, rows]);

  const commitName = () => {
    const value = draft.trim();
    nameCluster(cluster.cluster_id, value).catch(() => {});
    setName(value || null);
    setEditing(false);
  };

  // "Not this person": detach their face(s) in that photo, optimistically remove
  // the cell, and offer a single-level Undo.
  const undoTimer = useRef<number | undefined>(undefined);
  const remove = (photo: PhotoRow) => {
    setRows((rs) => rs.filter((r) => r.id !== photo.id));
    removePersonFace(photo.id, cluster.cluster_id)
      .then((faceIds) => {
        setUndo({ row: photo, faceIds });
        if (undoTimer.current) window.clearTimeout(undoTimer.current);
        undoTimer.current = window.setTimeout(() => setUndo(null), 6000);
      })
      .catch(() => {
        // Restore the cell if the backend rejected the change.
        setRows((rs) => insertSorted(rs, photo));
      });
  };
  const doUndo = () => {
    if (!undo) return;
    const { row, faceIds } = undo;
    setUndo(null);
    restorePersonFaces(faceIds, cluster.cluster_id)
      .then(() => setRows((rs) => insertSorted(rs, row)))
      .catch(() => {});
  };

  const header = (
    <div className="person-header">
      <button className="ghost-btn person-back" onClick={onBack}>
        ‹ Back
      </button>
      <img className="person-avatar" src={faceCropUrl(cluster.cover_face_id)} alt="" draggable={false} />
      <div className="person-meta">
        {editing ? (
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
          {total.toLocaleString()} {total === 1 ? "photo" : "photos"}
          {total > 0 && (
            <>
              {" · "}
              {(() => {
                const lo = monthYear(rows[total - 1].ts);
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

      {loaded && total === 0 ? (
        <div className="empty">
          <p>No photos left for this person.</p>
          <button className="ghost-btn" onClick={onBack}>
            ‹ Back to People
          </button>
        </div>
      ) : (
        <div className="grid-wrap">
          <div ref={scrollRef} className="grid-scroll">
            <div style={{ height: `${rowVirtualizer.getTotalSize()}px`, width: "100%", position: "relative" }}>
              {virtualRows.map((virtualRow) => {
                const rowStart = virtualRow.index * columns;
                const cells = [];
                for (let c = 0; c < columns; c++) {
                  const index = rowStart + c;
                  if (index >= total) break;
                  const photo = rows[index];
                  cells.push(
                    <div
                      key={photo.id}
                      className="cell person-cell"
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
                      <button
                        className="person-remove"
                        title={name ? `Not ${name}` : "Not this person"}
                        aria-label={name ? `Not ${name}` : "Not this person"}
                        onClick={(e) => {
                          e.stopPropagation();
                          remove(photo);
                        }}
                      >
                        ✕
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

      {undo && (
        <div className="undo-toast">
          <span>Removed</span>
          <button className="undo-btn" onClick={doUndo}>
            Undo
          </button>
        </div>
      )}

      {viewerIndex !== null && (
        <Lightbox index={viewerIndex} total={total} resolveId={resolveId} onClose={() => setViewerIndex(null)} />
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
