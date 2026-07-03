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
  getClusterGeneration,
  getClusters,
  getFacesInPhoto,
  getPhotoDetail,
  ignoreFaces,
  mergeClusters,
  nameCluster,
  onClusterProgress,
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
  const [stageSize, setStageSize] = useState({ width: 0, height: 0 });
  const [openFace, setOpenFace] = useState<number | null>(null);
  const [people, setPeople] = useState<Cluster[]>([]);
  // A toast for the last correction. `onUndo` is null for actions we don't reverse
  // (cluster merges, matching the People grid — re-split via the grid if needed).
  const [undo, setUndo] = useState<{ label: string; onUndo: (() => void) | null } | null>(null);
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

  // The people list AND the clustering generation its ids belong to. The viewer can
  // stay open across a background re-cluster (which renumbers cluster ids), so both
  // refresh when one finishes; every cluster-targeting mutation passes the generation
  // so a stale id is refused instead of naming/merging whatever cluster now holds it.
  const genRef = useRef(0);
  useEffect(() => {
    const refreshPeople = () => {
      getClusters().then(setPeople).catch(() => {});
      getClusterGeneration()
        .then((g) => {
          genRef.current = g;
        })
        .catch(() => {});
    };
    refreshPeople();
    let unlisten: (() => void) | undefined;
    onClusterProgress((p) => {
      if (!p.running) refreshPeople();
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
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

  // Cluster-level: name this person, or merge their group into an existing person —
  // the unified "identify this unnamed face" flow (mirrors the People grid).
  const nameThisPerson = useCallback(
    async (face: PhotoFace, name: string) => {
      setOpenFace(null);
      if (face.cluster_id == null || !name) return;
      const prev = face.name ?? "";
      try {
        // Naming may promote the group to a new (negative, stable) key — undo
        // must rename THAT group, not the possibly-dead positive id.
        const g = await nameCluster(face.cluster_id, name, genRef.current);
        flashUndo(`Named ${name}`, () =>
          nameCluster(g, prev, genRef.current).then(afterChange).catch(() => {}),
        );
        afterChange();
      } catch {
        flashRefused();
      }
    },
    [flashUndo, afterChange, flashRefused],
  );
  const mergeThisPerson = useCallback(
    async (face: PhotoFace, target: Cluster) => {
      setOpenFace(null);
      if (face.cluster_id == null) return;
      try {
        await mergeClusters(target.cluster_id, face.cluster_id, genRef.current);
        flashUndo(`Merged into ${target.name}`, null); // a merge isn't cleanly reversible
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
                    placement={menuPlacement(box, stageSize)}
                    onNamePerson={(name) => nameThisPerson(f, name)}
                    onMergePerson={(target) => mergeThisPerson(f, target)}
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

      {undo && (
        <div className="undo-toast">
          <span>{undo.label}</span>
          {undo.onUndo && (
            <button className="undo-btn" onClick={doUndo}>
              Undo
            </button>
          )}
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

// Where to anchor a face's menu so it stays on-screen: below the box by default,
// flipped above when there isn't room, and clamped horizontally to the stage.
type Placement = { left: number; top: number } | { left: number; bottom: number };
const MENU_W = 232;
const MENU_H = 300; // generous estimate incl. the matches list
function menuPlacement(box: ContentRect, stage: { width: number; height: number }): Placement {
  const left = Math.max(8, Math.min(box.left, stage.width - MENU_W - 8));
  const below = stage.height - (box.top + box.height);
  if (below >= MENU_H || below >= box.top) {
    return { left, top: box.top + box.height + 6 };
  }
  return { left, bottom: stage.height - box.top + 6 };
}

// The popover for one face. For an UNNAMED person it's a single combobox — type a
// new name (names this person), or pick an existing person (merges into them) — so
// the user never has to remember whether they've named someone before. For a NAMED
// person the scopes differ (rename the person vs. move just this face), so those
// stay as distinct actions.
function FaceMenu({
  face,
  people,
  placement,
  onNamePerson,
  onMergePerson,
  onReassignExisting,
  onReassignNew,
  onIgnore,
  onClose,
}: {
  face: PhotoFace;
  people: Cluster[];
  placement: Placement;
  onNamePerson: (name: string) => void;
  onMergePerson: (target: Cluster) => void;
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

  const q = draft.trim().toLowerCase();
  const matches = people
    .filter((c) => c.cluster_id !== face.cluster_id && c.name)
    .filter((c) => (q ? c.name!.toLowerCase().includes(q) : true))
    .slice(0, 6);
  const exact = q ? people.find((c) => c.name && c.name.toLowerCase() === q) : undefined;

  return (
    <div className="face-menu" style={placement} onClick={(e) => e.stopPropagation()}>
      {/* Unnamed: one unified "name or pick a person" combobox. */}
      {unnamed && (
        <div className="fm-move">
          <input
            className="pname-input fm-input"
            autoFocus
            value={draft}
            placeholder="Name, or pick a person"
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") onClose();
              else if (e.key === "Enter" && q) (exact ? onMergePerson(exact) : onNamePerson(draft.trim()));
            }}
          />
          <ul className="fm-matches">
            {matches.map((m) => (
              <li key={m.cluster_id} className="fm-match" onClick={() => onMergePerson(m)}>
                <img className="ns-face" src={faceCropUrl(m.cover_face_id)} alt="" draggable={false} />
                <span className="ns-name">{m.name}</span>
              </li>
            ))}
            {draft.trim() && !exact && (
              <li className="fm-match fm-new" onClick={() => onNamePerson(draft.trim())}>
                + Name “{draft.trim()}”
              </li>
            )}
            <li className="fm-match danger" onClick={onIgnore}>
              Ignore this face
            </li>
          </ul>
        </div>
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
