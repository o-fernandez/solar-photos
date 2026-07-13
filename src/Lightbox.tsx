// The immersive photo viewer (Principle: foreground wins, never lose your place).
//
// Opens over the grid as a full-window overlay. ←/→ move through the library,
// Esc closes back to the exact grid position (the grid stays mounted underneath,
// untouched). The image is the decoded, EXIF-oriented preview served by Rust
// over photo://; neighbors are prefetched so arrowing feels instant.
//
// Over the photo we draw the detected faces, each a box you can act on: name the
// person, say "this is someone else" (reassign), or ignore the face. The boxes are
// only drawn for the photo in focus, so nothing in the grid behind reflows (P2).

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import {
  confirmFacesIntoCluster,
  getFacesInPhoto,
  getPhotoDetail,
  getPhotoExif,
  ignoreFaces,
  setPhotoFavorite,
  setPhotoHidden,
  nameCluster,
  nameFaces,
  photoUrl,
  reassignFacesToCluster,
  reassignFacesToNewPerson,
  revealInFinder,
  undoCorrection,
  type Cluster,
  type CorrectionUndo,
  type PhotoDetail,
  type PhotoExif,
  type PhotoFace,
} from "./api";
import PersonPicker from "./PersonPicker";
import UndoToast from "./UndoToast";
import { fold } from "./fold";
import { fmtBytes } from "./format";
import { usePeopleDirectory } from "./usePeopleDirectory";

interface Props {
  index: number;
  total: number;
  /** Resolve the photo id at a library index (may fetch if not loaded yet). */
  resolveId: (index: number) => Promise<number | null>;
  onClose: () => void;
  /** Called after a face correction lands, so the opener can refresh its grid. */
  onCorrection?: () => void;
}

function formatWhen(ts: number): string {
  const d = new Date(ts * 1000);
  const date = d.toLocaleDateString(undefined, { day: "numeric", month: "long", year: "numeric" });
  const time = d.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
  return `${date} · ${time}`;
}

// The on-screen rect of an `object-fit: contain` image's actual content, relative
// to the image element — so normalized face boxes can be placed over it.
interface ContentRect {
  left: number;
  top: number;
  width: number;
  height: number;
}
function contentRectOf(img: HTMLImageElement): ContentRect | null {
  const { naturalWidth: nw, naturalHeight: nh, clientWidth: bw, clientHeight: bh } = img;
  if (!nw || !nh || !bw || !bh) return null;
  const scale = Math.min(bw / nw, bh / nh);
  const w = nw * scale;
  const h = nh * scale;
  return { left: (bw - w) / 2, top: (bh - h) / 2, width: w, height: h };
}

export default function Lightbox({ index, total, resolveId, onClose, onCorrection }: Props) {
  const [current, setCurrent] = useState(index);
  const [id, setId] = useState<number | null>(null);
  // The photo actually on screen. Navigation preloads the next photo off-screen
  // and swaps `shownId` only once it's decoded — the previous photo stays up, so
  // arrowing never flashes to black even when the neighbor isn't cached yet.
  const [shownId, setShownId] = useState<number | null>(null);
  const [detail, setDetail] = useState<PhotoDetail | null>(null);
  const [loading, setLoading] = useState(true);
  // The info card (press I, or click the caption): the caption bar grows
  // upward into camera/file/location rows. Stays open while arrowing — the
  // EXIF refetches per photo. Read on demand only; never at scan time.
  const [showInfo, setShowInfo] = useState(false);
  const [exifInfo, setExifInfo] = useState<PhotoExif | null>(null);
  // Face boxes can be hidden (F, or the button) to look at the photo clean.
  const [showFaces, setShowFaces] = useState(true);
  // Zoom/pan: pinch (ctrl+wheel) or double-click zooms toward the cursor; while
  // zoomed, scroll or drag pans and 0 (or double-click) snaps back to fit. Reset
  // on every navigation. Face boxes draw only at fit — their overlay is measured
  // against the untransformed image.
  const [zoom, setZoom] = useState(1);
  const [pan, setPan] = useState({ x: 0, y: 0 });
  const zoomRef = useRef(1);
  const panRef = useRef({ x: 0, y: 0 });

  const [faces, setFaces] = useState<PhotoFace[]>([]);
  // The viewer can stay open across a background re-cluster (which renumbers
  // cluster ids) — the hook keeps the people list + generation fresh, and every
  // cluster-targeting mutation passes the generation so a stale id is refused.
  const { people, genRef } = usePeopleDirectory();
  const [rect, setRect] = useState<ContentRect | null>(null);
  const [stageSize, setStageSize] = useState({ width: 0, height: 0 });
  // Read by the (stable) zoom/pan handlers without re-binding per render.
  const rectRef = useRef<ContentRect | null>(null);
  rectRef.current = rect;
  const stageSizeRef = useRef(stageSize);
  stageSizeRef.current = stageSize;
  const [openFace, setOpenFace] = useState<number | null>(null);
  // A toast for the last correction. `onUndo` is null for actions we don't reverse
  // (cluster merges, matching the People grid — re-split via the grid if needed).
  const [undo, setUndo] = useState<{ label: string; onUndo: (() => void) | null } | null>(null);
  const imgRef = useRef<HTMLImageElement>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const undoTimer = useRef<number | undefined>(undefined);

  // Clamp + commit a zoom/pan pair (fit snaps the pan home). Refs mirror the
  // state so wheel/drag handlers stay stable across renders.
  const setView = useCallback((z: number, p: { x: number; y: number }) => {
    const zc = Math.min(6, Math.max(1, z));
    let pc = { x: 0, y: 0 };
    if (zc > 1) {
      const r = rectRef.current;
      const s = stageSizeRef.current;
      const maxX = r && s.width ? Math.max(0, (r.width * zc - s.width) / 2) + 24 : 0;
      const maxY = r && s.height ? Math.max(0, (r.height * zc - s.height) / 2) + 24 : 0;
      pc = {
        x: Math.min(maxX, Math.max(-maxX, p.x)),
        y: Math.min(maxY, Math.max(-maxY, p.y)),
      };
    }
    zoomRef.current = zc;
    panRef.current = pc;
    setZoom(zc);
    setPan(pc);
  }, []);

  const go = useCallback(
    (delta: number) => {
      setOpenFace(null);
      setView(1, { x: 0, y: 0 });
      setCurrent((c) => Math.min(total - 1, Math.max(0, c + delta)));
    },
    [total, setView],
  );

  // Keyboard: ←/→ navigate, F toggles face boxes, I toggles the info card, Esc
  // walks back (face menu → info card → close). Ignored while typing in a
  // face-menu field — the fields handle their own keys, and an arrow press
  // there must not change the photo.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA")) return;
      if (e.key === "Escape") {
        if (openFace !== null) setOpenFace(null);
        else if (showInfo) setShowInfo(false);
        else onClose();
      } else if (e.key === "ArrowRight") go(1);
      else if (e.key === "ArrowLeft") go(-1);
      else if (e.key.toLowerCase() === "f") {
        setOpenFace(null);
        setShowFaces((s) => !s);
      } else if (e.key.toLowerCase() === "i") setShowInfo((s) => !s);
      else if (e.key === "0") setView(1, { x: 0, y: 0 });
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [go, onClose, openFace, showInfo, setView]);

  // Fetch the open photo's EXIF whenever the card is up (and re-fetch as the
  // user arrows through with it open).
  useEffect(() => {
    if (!showInfo || id == null) return;
    let alive = true;
    setExifInfo(null);
    getPhotoExif(id)
      .then((x) => {
        if (alive) setExifInfo(x);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [showInfo, id]);

  // Pinch (ctrl/cmd+wheel) zooms toward the cursor; plain scroll pans while
  // zoomed. Native listener — React's synthetic wheel is passive, and zooming
  // must preventDefault or the page rubber-bands.
  useEffect(() => {
    const stage = stageRef.current;
    if (!stage) return;
    const onWheel = (e: WheelEvent) => {
      const z1 = zoomRef.current;
      if (e.ctrlKey || e.metaKey) {
        e.preventDefault();
        const r = stage.getBoundingClientRect();
        const cx = e.clientX - r.left - r.width / 2;
        const cy = e.clientY - r.top - r.height / 2;
        const z2 = Math.min(6, Math.max(1, z1 * Math.exp(-e.deltaY * 0.01)));
        if (z2 === z1) return;
        const scale = z2 / z1;
        const p1 = panRef.current;
        setOpenFace(null);
        setView(z2, { x: cx - scale * (cx - p1.x), y: cy - scale * (cy - p1.y) });
      } else if (z1 > 1) {
        e.preventDefault();
        const p1 = panRef.current;
        setView(z1, { x: p1.x - e.deltaX, y: p1.y - e.deltaY });
      }
    };
    stage.addEventListener("wheel", onWheel, { passive: false });
    return () => stage.removeEventListener("wheel", onWheel);
  }, [setView]);

  // Double-click: zoom in toward the click, or snap back to fit.
  const onStageDoubleClick = useCallback(
    (e: React.MouseEvent) => {
      const stage = stageRef.current;
      if (!stage) return;
      if (zoomRef.current > 1) {
        setView(1, { x: 0, y: 0 });
        return;
      }
      const r = stage.getBoundingClientRect();
      const cx = e.clientX - r.left - r.width / 2;
      const cy = e.clientY - r.top - r.height / 2;
      setOpenFace(null);
      setView(2.5, { x: cx * (1 - 2.5), y: cy * (1 - 2.5) });
    },
    [setView],
  );

  // Drag pans while zoomed.
  const onStagePointerDown = useCallback(
    (e: React.PointerEvent) => {
      if (zoomRef.current <= 1 || e.button !== 0) return;
      e.preventDefault();
      const startX = e.clientX;
      const startY = e.clientY;
      const p0 = { ...panRef.current };
      const move = (ev: PointerEvent) =>
        setView(zoomRef.current, { x: p0.x + ev.clientX - startX, y: p0.y + ev.clientY - startY });
      const up = () => {
        window.removeEventListener("pointermove", move);
        window.removeEventListener("pointerup", up);
      };
      window.addEventListener("pointermove", move);
      window.addEventListener("pointerup", up);
    },
    [setView],
  );

  // Resolve the current photo's id + detail + faces, and prefetch neighbors.
  useEffect(() => {
    let alive = true;
    setLoading(true);
    setFaces([]);
    setRect(null);
    resolveId(current).then((resolved) => {
      if (!alive) return;
      setId(resolved);
      if (resolved != null) {
        getPhotoDetail(resolved).then((d) => alive && setDetail(d));
        getFacesInPhoto(resolved).then((f) => alive && setFaces(f)).catch(() => {});
      }
    });
    [current - 1, current + 1, current - 2, current + 2].forEach((i) => {
      if (i < 0 || i >= total || i === current) return;
      resolveId(i).then((nid) => {
        if (nid != null) {
          const img = new Image();
          img.src = photoUrl(nid);
        }
      });
    });
    return () => {
      alive = false;
    };
  }, [current, total, resolveId]);

  // Swap the on-screen photo only once the new one is decoded (see `shownId`).
  // A slow preload that lands after the user has arrowed further is dropped —
  // only the *latest* target may swap in.
  const idRef = useRef<number | null>(null);
  idRef.current = id;
  useEffect(() => {
    if (id == null) return;
    if (shownId == null) {
      setShownId(id); // first photo: the <img> itself shows progress
      return;
    }
    if (id === shownId) {
      // Navigated away and straight back: the <img> src won't change, so no load
      // event will clear the spinner — the photo on screen is already right.
      setLoading(false);
      return;
    }
    const pre = new Image();
    const swap = () => {
      if (idRef.current === id) setShownId(id); // onerror too: let the <img> surface it
    };
    pre.onload = swap;
    pre.onerror = swap;
    pre.src = photoUrl(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id]);

  // Keep the face-box overlay aligned to the displayed image as it loads / resizes.
  // The boxes are children of the stage, so we express the image's content rect in
  // the stage's coordinates (image offset within the padded, centered stage + the
  // object-fit contain offset, which is ~0 since the <img> shrink-wraps its image).
  const remeasure = useCallback(() => {
    // Fit-only: getBoundingClientRect includes the zoom transform, so measuring
    // while zoomed would corrupt the overlay geometry. Boxes only draw at fit.
    if (zoomRef.current !== 1) return;
    const img = imgRef.current;
    const stage = stageRef.current;
    if (!img || !stage) return;
    setStageSize({ width: stage.clientWidth, height: stage.clientHeight });
    const cr = contentRectOf(img);
    if (!cr) return;
    const ir = img.getBoundingClientRect();
    const sr = stage.getBoundingClientRect();
    setRect({
      left: ir.left - sr.left + cr.left,
      top: ir.top - sr.top + cr.top,
      width: cr.width,
      height: cr.height,
    });
  }, []);
  useLayoutEffect(() => {
    const onResize = () => remeasure();
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, [remeasure]);
  // A resize that happened while zoomed re-measures once we're back at fit.
  useEffect(() => {
    if (zoom === 1) remeasure();
  }, [zoom, remeasure]);

  // Refetch the faces for the current photo (after a correction changes them).
  const refreshFaces = useCallback(() => {
    if (id != null) getFacesInPhoto(id).then(setFaces).catch(() => {});
  }, [id]);

  const flashUndo = useCallback((label: string, onUndo: (() => void) | null) => {
    setUndo({ label, onUndo });
    if (undoTimer.current) window.clearTimeout(undoTimer.current);
    undoTimer.current = window.setTimeout(() => setUndo(null), 6000);
  }, []);
  const afterChange = useCallback(() => {
    refreshFaces();
    onCorrection?.();
  }, [refreshFaces, onCorrection]);

  // On refusal (almost always a stale generation — a background re-cluster
  // renumbered ids under the open viewer), say so and refresh instead of failing
  // silently: a swallowed "Move to X" reads as "my correction didn't register".
  const flashRefused = useCallback(() => {
    flashUndo("People were just reorganized — try that again.", null);
    afterChange();
  }, [flashUndo, afterChange]);

  // A face-level correction (reassign / ignore) — reversible via its undo token.
  const applyToFace = useCallback(
    async (run: () => Promise<CorrectionUndo>, label: string) => {
      setOpenFace(null);
      try {
        const tok = await run();
        flashUndo(label, () => undoCorrection(tok).then(afterChange).catch(() => {}));
        afterChange();
      } catch {
        flashRefused();
      }
    },
    [flashUndo, afterChange, flashRefused],
  );

  // Renaming a NAMED person from their face — still cluster-scoped on purpose:
  // renaming changes the person's label, not any face's grouping.
  const nameThisPerson = useCallback(
    async (face: PhotoFace, name: string) => {
      setOpenFace(null);
      if (face.cluster_id == null || !name) return;
      try {
        // The backend token restores the prior name AND the confirmed flags the
        // naming wrote — renaming back by hand left the vouching behind.
        const { undo } = await nameCluster(face.cluster_id, name, genRef.current);
        flashUndo(`Named ${name}`, () =>
          undoCorrection(undo).then(afterChange).catch(() => {}),
        );
        afterChange();
      } catch {
        flashRefused();
      }
    },
    [flashUndo, afterChange, flashRefused],
  );

  const doUndo = () => {
    const fn = undo?.onUndo;
    setUndo(null);
    fn?.();
  };

  // Favorite / hide from the viewer: flip the flag on the open photo. Optimistic
  // (updates `detail` immediately), then persists and tells the opener so the grid
  // underneath reflects it — a hidden photo leaves the timeline on close, a star
  // fills its cell. Files are never touched.
  const toggleFavorite = () => {
    if (id == null || !detail) return;
    const next = !detail.favorite;
    setDetail({ ...detail, favorite: next });
    setPhotoFavorite(id, next).then(() => onCorrection?.()).catch(() => {});
  };
  const toggleHidden = () => {
    if (id == null || !detail) return;
    const next = !detail.hidden;
    setDetail({ ...detail, hidden: next });
    flashUndo(next ? "Hidden from the timeline" : "Restored to the timeline", () => {
      if (id != null) setPhotoHidden(id, !next).then(() => onCorrection?.()).catch(() => {});
      setDetail((d) => (d ? { ...d, hidden: !next } : d));
    });
    setPhotoHidden(id, next).then(() => onCorrection?.()).catch(() => {});
  };

  const when = detail ? formatWhen(detail.timestamp) : "";

  return (
    <div className="viewer" onClick={onClose}>
      <button
        className="viewer-btn viewer-close"
        aria-label="Close"
        onClick={(e) => {
          e.stopPropagation();
          onClose();
        }}
      >
        ✕
      </button>

      {detail && (
        <>
          <button
            className={`viewer-btn viewer-fav${detail.favorite ? " on" : ""}`}
            aria-label={detail.favorite ? "Remove favorite" : "Favorite"}
            aria-pressed={detail.favorite}
            title={detail.favorite ? "Remove favorite" : "Favorite"}
            onClick={(e) => {
              e.stopPropagation();
              toggleFavorite();
            }}
          >
            <svg width="21" height="21" viewBox="0 0 24 24" fill={detail.favorite ? "currentColor" : "none"} stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
              <path d="M12 20.3l-1.45-1.32C5.4 14.24 2 11.16 2 7.5 2 4.42 4.42 2 7.5 2c1.74 0 3.41.81 4.5 2.09C13.09 2.81 14.76 2 16.5 2 19.58 2 22 4.42 22 7.5c0 3.66-3.4 6.74-8.55 11.49L12 20.3z" />
            </svg>
          </button>
          <button
            className="viewer-btn viewer-hide"
            aria-label={detail.hidden ? "Restore to timeline" : "Hide from timeline"}
            title={detail.hidden ? "Restore to timeline" : "Hide from timeline"}
            onClick={(e) => {
              e.stopPropagation();
              toggleHidden();
            }}
          >
            {detail.hidden ? (
              <svg width="21" height="21" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
                <path d="M2 12s3-8 10-8 10 8 10 8-3 8-10 8-10-8-10-8z" />
                <circle cx="12" cy="12" r="3" />
              </svg>
            ) : (
              <svg width="21" height="21" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
                <path d="M9.9 4.24A9.1 9.1 0 0 1 12 4c7 0 10 8 10 8a18 18 0 0 1-2.16 3.19M6.6 6.6A18 18 0 0 0 2 12s3 8 10 8a9 9 0 0 0 5.4-1.6" />
                <path d="M1 1l22 22" />
              </svg>
            )}
          </button>
        </>
      )}

      {faces.length > 0 && (
        <button
          className={`viewer-btn viewer-faces${showFaces ? " on" : ""}`}
          aria-label={showFaces ? "Hide face boxes" : "Show face boxes"}
          aria-pressed={showFaces}
          title={showFaces ? "Hide faces (F)" : "Show faces (F)"}
          onClick={(e) => {
            e.stopPropagation();
            setOpenFace(null);
            setShowFaces((s) => !s);
          }}
        >
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <circle cx="12" cy="8" r="4" />
            <path d="M4 21c0-4 3.6-7 8-7s8 3 8 7" />
          </svg>
        </button>
      )}

      {current > 0 && (
        <button
          className="viewer-btn viewer-prev"
          aria-label="Previous"
          onClick={(e) => {
            e.stopPropagation();
            go(-1);
          }}
        >
          ‹
        </button>
      )}
      {current < total - 1 && (
        <button
          className="viewer-btn viewer-next"
          aria-label="Next"
          onClick={(e) => {
            e.stopPropagation();
            go(1);
          }}
        >
          ›
        </button>
      )}

      <div
        className={`viewer-stage${zoom > 1 ? " zoomed" : ""}`}
        ref={stageRef}
        onClick={(e) => {
          e.stopPropagation();
          if (openFace !== null) setOpenFace(null); // click-away closes the menu
        }}
        onDoubleClick={onStageDoubleClick}
        onPointerDown={onStagePointerDown}
      >
        {loading && <span className="viewer-spinner" />}
        {shownId != null && (
          <img
            ref={imgRef}
            src={photoUrl(shownId)}
            className="viewer-img"
            alt={detail?.filename ?? ""}
            draggable={false}
            style={
              zoom !== 1
                ? { transform: `translate(${pan.x}px, ${pan.y}px) scale(${zoom})` }
                : undefined
            }
            onLoad={() => {
              setLoading(false);
              remeasure();
            }}
            onError={() => setLoading(false)}
          />
        )}

        {/* Face boxes, positioned over the displayed image content — only for the
            photo actually on screen, at fit, and while the overlay is shown. */}
        {!loading && rect && showFaces && zoom === 1 && shownId === id &&
          faces.map((f) => {
            const box = {
              left: rect.left + f.x1 * rect.width,
              top: rect.top + f.y1 * rect.height,
              width: (f.x2 - f.x1) * rect.width,
              height: (f.y2 - f.y1) * rect.height,
            };
            return (
              <div key={f.face_id}>
                <button
                  className={`face-box${openFace === f.face_id ? " active" : ""}`}
                  style={box}
                  title={f.name ?? "Unnamed — click to label"}
                  onClick={(e) => {
                    e.stopPropagation();
                    setOpenFace((o) => (o === f.face_id ? null : f.face_id));
                  }}
                >
                  <span className="face-tag">{f.name ?? "Unnamed"}</span>
                </button>
                {openFace === f.face_id && (
                  <FaceMenu
                    face={f}
                    people={people}
                    placement={menuPlacement(box, stageSize)}
                    onNamePerson={(name) => nameThisPerson(f, name)}
                    onNameFaceOnly={(name) =>
                      applyToFace(
                        () => nameFaces([f.face_id], name, genRef.current),
                        `Named this face ${name}`,
                      )
                    }
                    onConfirmFaceInto={(target) =>
                      applyToFace(
                        () =>
                          confirmFacesIntoCluster([f.face_id], target.cluster_id, genRef.current),
                        `Moved this face to ${target.name}`,
                      )
                    }
                    onReassignExisting={(target) =>
                      applyToFace(
                        () =>
                          reassignFacesToCluster(
                            [f.face_id],
                            f.cluster_id!,
                            target.cluster_id,
                            genRef.current,
                          ),
                        `Moved to ${target.name}`,
                      )
                    }
                    onReassignNew={(name) =>
                      applyToFace(
                        () =>
                          reassignFacesToNewPerson(
                            [f.face_id],
                            f.cluster_id!,
                            name,
                            genRef.current,
                          ),
                        name ? `Moved to ${name}` : "Moved to a new person",
                      )
                    }
                    onIgnore={() => applyToFace(() => ignoreFaces([f.face_id]), "Face ignored")}
                    onClose={() => setOpenFace(null)}
                  />
                )}
              </div>
            );
          })}
      </div>

      {undo && <UndoToast label={undo.label} onUndo={undo.onUndo ? doUndo : undefined} />}

      {detail && shownId === id && !showInfo && (
        <div
          className="viewer-caption clickable"
          title="Details (I)"
          onClick={(e) => {
            e.stopPropagation();
            setShowInfo(true);
          }}
        >
          <span className="viewer-captext">
            <span className="viewer-counter">
              {(current + 1).toLocaleString()} of {total.toLocaleString()} ·{" "}
            </span>
            {when}
            <span className="viewer-filename"> · {detail.filename}</span>
          </span>
          <button
            className="viewer-reveal"
            title="Show this file in Finder"
            onClick={(e) => {
              e.stopPropagation();
              revealInFinder(detail.path).catch(() => {});
            }}
          >
            Show in Finder
          </button>
        </div>
      )}

      {/* The caption, grown into the info card (Direction A: the photo never
          moves; the card floats where the caption was). */}
      {detail && shownId === id && showInfo && (
        <div className="viewer-info" onClick={(e) => e.stopPropagation()}>
          <div
            className="vi-row vi-head"
            title="Collapse (I)"
            onClick={() => setShowInfo(false)}
          >
            <span>{when}</span>
            <span className="vi-counter">
              {(current + 1).toLocaleString()} of {total.toLocaleString()}
            </span>
          </div>
          <div className="vi-row">
            <span className="vi-file" title={detail.path}>
              {detail.filename}
            </span>
            <span className="vi-side">
              {exifInfo ? fmtBytes(exifInfo.bytes) : ""}
              <button
                className="viewer-reveal"
                onClick={() => revealInFinder(detail.path).catch(() => {})}
              >
                Show in Finder
              </button>
            </span>
          </div>
          {exifInfo == null ? (
            <div className="vi-row vi-note">Reading details…</div>
          ) : exifInfo.cloud ? (
            <div className="vi-row vi-note">
              Kept in the cloud — camera details load once this photo downloads
            </div>
          ) : (
            <>
              <div className="vi-row">
                <span className="vi-lbl">
                  {exifInfo.camera ?? "Unknown camera"}
                  {exifInfo.lens ? ` · ${exifInfo.lens}` : ""}
                </span>
                <span>
                  {[exifInfo.f_number, exifInfo.exposure, exifInfo.iso, exifInfo.focal]
                    .filter(Boolean)
                    .join(" · ") || "—"}
                </span>
              </div>
              <div className="vi-row">
                <span className="vi-lbl">
                  {exifInfo.width && exifInfo.height
                    ? `${exifInfo.width.toLocaleString()} × ${exifInfo.height.toLocaleString()}`
                    : "Dimensions unknown"}
                </span>
                <span>
                  {exifInfo.gps
                    ? `${exifInfo.gps[0].toFixed(4)}, ${exifInfo.gps[1].toFixed(4)}`
                    : "no location recorded"}
                </span>
              </div>
            </>
          )}
        </div>
      )}
    </div>
  );
}

// Where to anchor a face's menu so it stays on-screen: below the box by default,
// flipped above when there isn't room, and clamped horizontally to the stage.
type Placement = { left: number; top: number } | { left: number; bottom: number };
const MENU_W = 232;
const MENU_H = 330; // generous estimate incl. the scope row + matches list
function menuPlacement(box: ContentRect, stage: { width: number; height: number }): Placement {
  const left = Math.max(8, Math.min(box.left, stage.width - MENU_W - 8));
  const below = stage.height - (box.top + box.height);
  if (below >= MENU_H || below >= box.top) {
    return { left, top: box.top + box.height + 6 };
  }
  return { left, bottom: stage.height - box.top + 6 };
}

// The popover for one face. For an UNNAMED person it's a single combobox — type a
// new name, or pick an existing person — and it applies to THIS FACE ONLY: you
// can't see the rest of the group from here, so the menu never asks you to vouch
// for it blind (naming one face once confirmed a 262-photo pose blob). The refold
// pulls the rest of the group in later, exactly when the confirmed evidence
// supports it. For a NAMED person the scopes differ (rename the person vs. move
// just this face), so those stay as distinct actions.
function FaceMenu({
  face,
  people,
  placement,
  onNamePerson,
  onNameFaceOnly,
  onConfirmFaceInto,
  onReassignExisting,
  onReassignNew,
  onIgnore,
  onClose,
}: {
  face: PhotoFace;
  people: Cluster[];
  placement: Placement;
  /** Rename a named person (cluster-scoped — a label change, not a face move). */
  onNamePerson: (name: string) => void;
  onNameFaceOnly: (name: string) => void;
  onConfirmFaceInto: (target: Cluster) => void;
  onReassignExisting: (target: Cluster) => void;
  onReassignNew: (name?: string) => void;
  onIgnore: () => void;
  onClose: () => void;
}) {
  const unnamed = face.name == null;
  // For an unnamed face we open straight into the combobox; for a named one we start
  // at the action menu and drop into "rename" or "move just this face" on demand.
  const [mode, setMode] = useState<"root" | "name" | "move">(unnamed ? "name" : "root");
  const [draft, setDraft] = useState("");

  // The exact-match probe: when the typed text IS an existing person (accents
  // aside), the "+ Name “X”" row is redundant (that person's own row commits
  // the same thing).
  const q = fold(draft.trim());
  const exact = q ? people.find((c) => c.name && fold(c.name) === q) : undefined;

  return (
    <div className="face-menu" style={placement} onClick={(e) => e.stopPropagation()}>
      {/* Unnamed: one unified "name or pick a person" combobox. The top match is
          Enter's default; "Ignore" stays click-only — too destructive for a
          stray Enter. */}
      {unnamed && (
        <PersonPicker
          variant="menu"
          people={people}
          excludeId={face.cluster_id}
          query={draft}
          onQueryChange={setDraft}
          placeholder="Name, or pick a person"
          hint="Applies to this face — look-alikes group up as Solar learns"
          matchAll
          onPick={onConfirmFaceInto}
          trailing={[
            ...(draft.trim() && !exact
              ? [
                  {
                    key: "new",
                    className: "fm-new",
                    content: <>+ Name “{draft.trim()}”</>,
                    onPick: () => onNameFaceOnly(draft.trim()),
                  },
                ]
              : []),
            {
              key: "ignore",
              className: "danger",
              content: "Ignore this face",
              onPick: onIgnore,
              nav: false,
            },
          ]}
          onEscape={onClose}
        />
      )}

      {/* Named: rename the person, move just this face, or ignore. */}
      {!unnamed && mode === "root" && (
        <>
          <div className="fm-head">{face.name}</div>
          <button className="fm-item" onClick={() => { setDraft(face.name ?? ""); setMode("name"); }}>
            Rename
          </button>
          <button className="fm-item" onClick={() => { setDraft(""); setMode("move"); }}>
            This is someone else…
          </button>
          <button className="fm-item danger" onClick={onIgnore}>
            Ignore this face
          </button>
        </>
      )}

      {!unnamed && mode === "name" && (
        <input
          className="pname-input fm-input"
          autoFocus
          value={draft}
          placeholder="Name"
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") onNamePerson(draft.trim());
            else if (e.key === "Escape") setMode("root");
          }}
          onBlur={() => onNamePerson(draft.trim())}
        />
      )}

      {!unnamed && mode === "move" && (
        <PersonPicker
          variant="menu"
          people={people}
          excludeId={face.cluster_id}
          query={draft}
          onQueryChange={setDraft}
          placeholder="Move to which person?"
          matchAll
          onPick={onReassignExisting}
          trailing={[
            {
              key: "new",
              className: "fm-new",
              content: <>+ New person{draft.trim() ? ` “${draft.trim()}”` : ""}</>,
              onPick: () => onReassignNew(draft.trim() || undefined),
            },
          ]}
          onEscape={() => setMode("root")}
        />
      )}

      <button className="fm-close" aria-label="Close" onClick={onClose}>
        ✕
      </button>
    </div>
  );
}
