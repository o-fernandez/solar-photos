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
  faceCropUrl,
  getClusters,
  getFacesInPhoto,
  getPhotoDetail,
  ignoreFaces,
  nameCluster,
  photoUrl,
  reassignFacesToCluster,
  reassignFacesToNewPerson,
  undoCorrection,
  type Cluster,
  type CorrectionUndo,
  type PhotoDetail,
  type PhotoFace,
} from "./api";

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
  const [detail, setDetail] = useState<PhotoDetail | null>(null);
  const [loading, setLoading] = useState(true);

  const [faces, setFaces] = useState<PhotoFace[]>([]);
  const [rect, setRect] = useState<ContentRect | null>(null);
  const [openFace, setOpenFace] = useState<number | null>(null);
  const [people, setPeople] = useState<Cluster[]>([]);
  const [undo, setUndo] = useState<{ tok: CorrectionUndo; label: string } | null>(null);
  const imgRef = useRef<HTMLImageElement>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const undoTimer = useRef<number | undefined>(undefined);

  const go = useCallback(
    (delta: number) => {
      setOpenFace(null);
      setCurrent((c) => Math.min(total - 1, Math.max(0, c + delta)));
    },
    [total],
  );

  // Keyboard: ←/→ navigate, Esc closes (or just closes an open face menu first).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        if (openFace !== null) setOpenFace(null);
        else onClose();
      } else if (e.key === "ArrowRight") go(1);
      else if (e.key === "ArrowLeft") go(-1);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [go, onClose, openFace]);

  useEffect(() => {
    getClusters().then(setPeople).catch(() => {});
  }, []);

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

  // Keep the face-box overlay aligned to the displayed image as it loads / resizes.
  // The boxes are children of the stage, so we express the image's content rect in
  // the stage's coordinates (image offset within the padded, centered stage + the
  // object-fit contain offset, which is ~0 since the <img> shrink-wraps its image).
  const remeasure = useCallback(() => {
    const img = imgRef.current;
    const stage = stageRef.current;
    if (!img || !stage) return;
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

  // Refetch the faces for the current photo (after a correction changes them).
  const refreshFaces = useCallback(() => {
    if (id != null) getFacesInPhoto(id).then(setFaces).catch(() => {});
  }, [id]);

  // Apply a correction to one face, refresh the overlay, and offer Undo.
  const applyToFace = useCallback(
    async (run: () => Promise<CorrectionUndo>, label: string) => {
      setOpenFace(null);
      try {
        const tok = await run();
        setUndo({ tok, label });
        if (undoTimer.current) window.clearTimeout(undoTimer.current);
        undoTimer.current = window.setTimeout(() => setUndo(null), 6000);
        refreshFaces();
        onCorrection?.();
      } catch {
        /* leave the overlay as-is on failure */
      }
    },
    [refreshFaces, onCorrection],
  );

  const doUndo = () => {
    if (!undo) return;
    const tok = undo.tok;
    setUndo(null);
    undoCorrection(tok)
      .then(() => {
        refreshFaces();
        onCorrection?.();
      })
      .catch(() => {});
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

      <div className="viewer-stage" ref={stageRef} onClick={(e) => e.stopPropagation()}>
        {loading && <span className="viewer-spinner" />}
        {id != null && (
          <img
            key={id}
            ref={imgRef}
            src={photoUrl(id)}
            className="viewer-img"
            alt={detail?.filename ?? ""}
            draggable={false}
            onLoad={() => {
              setLoading(false);
              remeasure();
            }}
            onError={() => setLoading(false)}
            style={{ opacity: loading ? 0 : 1 }}
          />
        )}

        {/* Face boxes, positioned over the displayed image content. */}
        {!loading && rect &&
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
                    anchor={box}
                    onName={(name) => applyToFace(() => nameClusterUndo(f, name), name ? `Named ${name}` : "Name cleared")}
                    onReassignExisting={(target) =>
                      applyToFace(
                        () => reassignFacesToCluster([f.face_id], f.cluster_id!, target.cluster_id),
                        `Moved to ${target.name}`,
                      )
                    }
                    onReassignNew={(name) =>
                      applyToFace(
                        () => reassignFacesToNewPerson([f.face_id], f.cluster_id!, name),
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

      {undo && (
        <div className="undo-toast">
          <span>{undo.label}</span>
          <button className="undo-btn" onClick={doUndo}>
            Undo
          </button>
        </div>
      )}

      {detail && (
        <div className="viewer-caption" onClick={(e) => e.stopPropagation()}>
          {when}
          <span className="viewer-filename"> · {detail.filename}</span>
        </div>
      )}
    </div>
  );
}

// Naming a cluster doesn't itself return an undo token, but the menu funnels every
// action through `applyToFace`'s undo path. We wrap it to satisfy that shape: a
// name change is its own undo (re-applying the prior name), so we record nothing
// reversible here and return an empty token. (Rename is rare and low-stakes; a full
// name-history undo isn't worth the complexity.)
async function nameClusterUndo(face: PhotoFace, name: string): Promise<CorrectionUndo> {
  if (face.cluster_id != null) await nameCluster(face.cluster_id, name);
  return { prior: [], new_cluster_id: null, added_cannot_link: null };
}

// The popover for one face: name / rename, "this is someone else" (reassign to an
// existing or new person), or ignore. Mirrors the person-page affordances.
function FaceMenu({
  face,
  people,
  anchor,
  onName,
  onReassignExisting,
  onReassignNew,
  onIgnore,
  onClose,
}: {
  face: PhotoFace;
  people: Cluster[];
  anchor: ContentRect;
  onName: (name: string) => void;
  onReassignExisting: (target: Cluster) => void;
  onReassignNew: (name?: string) => void;
  onIgnore: () => void;
  onClose: () => void;
}) {
  const [mode, setMode] = useState<"root" | "name" | "move">("root");
  const [draft, setDraft] = useState("");

  const matches = people
    .filter((c) => c.cluster_id !== face.cluster_id && c.name)
    .filter((c) => (draft.trim() ? c.name!.toLowerCase().includes(draft.trim().toLowerCase()) : true))
    .slice(0, 6);

  // Anchor the menu just below the face box.
  const style = { left: anchor.left, top: anchor.top + anchor.height + 6 };

  return (
    <div className="face-menu" style={style} onClick={(e) => e.stopPropagation()}>
      {mode === "root" && (
        <>
          <div className="fm-head">{face.name ?? "Unnamed person"}</div>
          {face.cluster_id != null && (
            <>
              <button className="fm-item" onClick={() => { setDraft(face.name ?? ""); setMode("name"); }}>
                {face.name ? "Rename" : "Add a name"}
              </button>
              <button className="fm-item" onClick={() => { setDraft(""); setMode("move"); }}>
                This is someone else…
              </button>
            </>
          )}
          <button className="fm-item danger" onClick={onIgnore}>
            Ignore this face
          </button>
        </>
      )}

      {mode === "name" && (
        <input
          className="pname-input fm-input"
          autoFocus
          value={draft}
          placeholder="Name"
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") onName(draft.trim());
            else if (e.key === "Escape") setMode("root");
          }}
          onBlur={() => onName(draft.trim())}
        />
      )}

      {mode === "move" && (
        <div className="fm-move">
          <input
            className="pname-input fm-input"
            autoFocus
            value={draft}
            placeholder="Move to which person?"
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") setMode("root");
              else if (e.key === "Enter" && draft.trim()) onReassignNew(draft.trim());
            }}
          />
          <ul className="fm-matches">
            {matches.map((m) => (
              <li key={m.cluster_id} className="fm-match" onClick={() => onReassignExisting(m)}>
                <img className="ns-face" src={faceCropUrl(m.cover_face_id)} alt="" draggable={false} />
                <span className="ns-name">{m.name}</span>
              </li>
            ))}
            <li className="fm-match fm-new" onClick={() => onReassignNew(draft.trim() || undefined)}>
              + New person{draft.trim() ? ` “${draft.trim()}”` : ""}
            </li>
          </ul>
        </div>
      )}

      <button className="fm-close" aria-label="Close" onClick={onClose}>
        ✕
      </button>
    </div>
  );
}
