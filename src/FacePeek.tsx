// A lightweight "see the full picture" overlay for review chips and cards.
//
// A tight face crop often isn't enough to answer "is this Mía?" — the context
// (who else is in the frame, where, roughly when) is the identifying signal. So
// any face crop in a review surface opens this: the full photo, the face in
// question outlined, click anywhere or Esc to dismiss — straight back to the
// card you were answering, nothing navigated, nothing lost.
//
// Esc is registered in the CAPTURE phase with stopPropagation so the peek wins
// over the surfaces underneath (the review session's Esc closes the whole
// session; the person page's Esc clears selections).

import { useCallback, useEffect, useRef, useState } from "react";
import { getFacePhoto, photoUrl, type FacePhoto } from "./api";

export default function FacePeek({
  faceId,
  onClose,
}: {
  faceId: number;
  onClose: () => void;
}) {
  const [info, setInfo] = useState<FacePhoto | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [box, setBox] = useState<{ left: number; top: number; width: number; height: number } | null>(
    null,
  );
  const imgRef = useRef<HTMLImageElement>(null);

  useEffect(() => {
    let alive = true;
    setInfo(null);
    setLoaded(false);
    setBox(null);
    getFacePhoto(faceId)
      .then((i) => {
        if (!alive) return;
        if (i) setInfo(i);
        else onClose(); // face vanished (re-detect) — nothing to show
      })
      .catch(() => onClose());
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [faceId]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  // The <img> is sized by max-width/height with its intrinsic aspect (no
  // object-fit letterboxing), so its client box IS the displayed image and the
  // normalized face box maps straight onto it.
  const measure = useCallback(() => {
    const img = imgRef.current;
    if (!img || !info || !img.naturalWidth) return;
    setBox({
      left: info.x1 * img.clientWidth,
      top: info.y1 * img.clientHeight,
      width: (info.x2 - info.x1) * img.clientWidth,
      height: (info.y2 - info.y1) * img.clientHeight,
    });
  }, [info]);

  useEffect(() => {
    const onResize = () => measure();
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, [measure]);

  return (
    <div
      className="peek-overlay"
      onClick={(e) => {
        e.stopPropagation();
        onClose();
      }}
    >
      {!loaded && <span className="viewer-spinner" />}
      {info && (
        <div className="peek-stage">
          <img
            ref={imgRef}
            className="peek-img"
            src={photoUrl(info.photo_id)}
            alt=""
            draggable={false}
            style={{ opacity: loaded ? 1 : 0 }}
            onLoad={() => {
              setLoaded(true);
              measure();
            }}
            onError={onClose}
          />
          {loaded && box && <div className="peek-facebox" style={box} />}
        </div>
      )}
    </div>
  );
}
