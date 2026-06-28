// The immersive photo viewer (Principle: foreground wins, never lose your place).
//
// Opens over the grid as a full-window overlay. ←/→ move through the library,
// Esc closes back to the exact grid position (the grid stays mounted underneath,
// untouched). The image is the decoded, EXIF-oriented preview served by Rust
// over photo://; neighbors are prefetched so arrowing feels instant.

import { useCallback, useEffect, useState } from "react";
import { getPhotoDetail, photoUrl, type PhotoDetail } from "./api";

interface Props {
  index: number;
  total: number;
  /** Resolve the photo id at a library index (may fetch if not loaded yet). */
  resolveId: (index: number) => Promise<number | null>;
  onClose: () => void;
}

function formatWhen(ts: number): string {
  const d = new Date(ts * 1000);
  const date = d.toLocaleDateString(undefined, { day: "numeric", month: "long", year: "numeric" });
  const time = d.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
  return `${date} · ${time}`;
}

export default function Lightbox({ index, total, resolveId, onClose }: Props) {
  const [current, setCurrent] = useState(index);
  const [id, setId] = useState<number | null>(null);
  const [detail, setDetail] = useState<PhotoDetail | null>(null);
  const [loading, setLoading] = useState(true);

  const go = useCallback(
    (delta: number) => {
      setCurrent((c) => Math.min(total - 1, Math.max(0, c + delta)));
    },
    [total],
  );

  // Keyboard: ←/→ navigate, Esc closes. Captured at the window so it works
  // regardless of focus.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
      else if (e.key === "ArrowRight") go(1);
      else if (e.key === "ArrowLeft") go(-1);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [go, onClose]);

  // Resolve the current photo's id + detail, and prefetch the neighbors so the
  // next arrow press shows instantly.
  useEffect(() => {
    let alive = true;
    setLoading(true);
    resolveId(current).then((resolved) => {
      if (!alive) return;
      setId(resolved);
      if (resolved != null) {
        getPhotoDetail(resolved).then((d) => alive && setDetail(d));
      }
    });
    // Prefetch ±1 (and ±2 lightly) — requesting the URL warms the Rust cache.
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

      <div className="viewer-stage" onClick={(e) => e.stopPropagation()}>
        {loading && <span className="viewer-spinner" />}
        {id != null && (
          <img
            key={id}
            src={photoUrl(id)}
            className="viewer-img"
            alt={detail?.filename ?? ""}
            draggable={false}
            onLoad={() => setLoading(false)}
            onError={() => setLoading(false)}
            style={{ opacity: loading ? 0 : 1 }}
          />
        )}
      </div>

      {detail && (
        <div className="viewer-caption" onClick={(e) => e.stopPropagation()}>
          {when}
          <span className="viewer-filename"> · {detail.filename}</span>
        </div>
      )}
    </div>
  );
}
