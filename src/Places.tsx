// The Places tab: every located photo on a world globe.
//
// Fully offline by design — the basemap is a PMTiles archive bundled as a Tauri
// resource (scripts/fetch-basemap.sh) and read by byte range over IPC; fonts and
// sprites ship in the app. No tile server is ever contacted: where you look is
// nobody's business (the same reason the library itself is local).
//
// Structure: a MapLibre globe (flattens as you zoom), photo-thumbnail cluster
// markers via supercluster, and a floating filmstrip of the photos in the
// current view — click a thumbnail (or a single-photo marker) to open the
// Lightbox scoped to that view. The whole dataset is held client-side
// ((id, lat, lon, ts) per photo — a few MB even at 100k) so panning never
// waits on the backend (P6).

import { useCallback, useEffect, useRef, useState } from "react";
import maplibregl from "maplibre-gl";
import "maplibre-gl/dist/maplibre-gl.css";
import Supercluster from "supercluster";
import Lightbox from "./Lightbox";
import { basemapSize, getGeoPoints, thumbUrl, type GeoPoint } from "./api";
import { basemapFlavor, basemapStyle, ensureBasemapProtocol } from "./basemap";

// How many markers we render per view (clusters keep it low anyway) and how
// many thumbs the filmstrip mounts (each is an <img> request — P6).
const MAX_MARKERS = 150;
const MAX_STRIP = 200;

type Status = "loading" | "nomap" | "ready";

interface ClusterProps {
  cluster?: boolean;
  cluster_id?: number;
  point_count?: number;
  id?: number;
  ts?: number;
}

export default function Places() {
  const containerRef = useRef<HTMLDivElement>(null);
  const mapRef = useRef<maplibregl.Map | null>(null);
  const indexRef = useRef<Supercluster<ClusterProps> | null>(null);
  const markersRef = useRef<maplibregl.Marker[]>([]);
  const pointsRef = useRef<GeoPoint[]>([]);
  // The photos in the current view, newest first — the filmstrip renders a
  // capped prefix; the Lightbox ranges over all of them.
  const inViewRef = useRef<GeoPoint[]>([]);

  const [status, setStatus] = useState<Status>("loading");
  const [pointCount, setPointCount] = useState(0);
  const [strip, setStrip] = useState<GeoPoint[]>([]);
  const [inViewCount, setInViewCount] = useState(0);
  const [viewerIndex, setViewerIndex] = useState<number | null>(null);
  // First map error, surfaced in the UI — a silent gray globe is undebuggable.
  const [mapError, setMapError] = useState<string | null>(null);

  // The year histogram + selected range (inclusive years; null = everything).
  // The whole dataset is client-side, so filtering just rebuilds the cluster
  // index over the subset — done when a drag ENDS, never per pointer move.
  const [years, setYears] = useState<{ y: number; n: number }[]>([]);
  const [range, setRange] = useState<[number, number] | null>(null);
  const [shownCount, setShownCount] = useState(0);
  const shownRef = useRef<GeoPoint[]>([]);
  const barsRef = useRef<HTMLDivElement>(null);

  const resolveId = useCallback(
    (i: number): Promise<number | null> => Promise.resolve(inViewRef.current[i]?.id ?? null),
    [],
  );

  /// Pack a point set into a fresh cluster index (null when empty).
  const buildIndex = useCallback((pts: GeoPoint[]) => {
    if (pts.length === 0) {
      indexRef.current = null;
      return;
    }
    const index = new Supercluster<ClusterProps>({ radius: 64, maxZoom: 17, minPoints: 2 });
    index.load(
      pts.map((p) => ({
        type: "Feature" as const,
        geometry: { type: "Point" as const, coordinates: [p.lon, p.lat] },
        properties: { id: p.id, ts: p.ts },
      })),
    );
    indexRef.current = index;
  }, []);

  // Recompute markers + filmstrip for the current viewport. Reads refs only, so
  // the single moveend listener never goes stale.
  const refreshView = useCallback(() => {
    const map = mapRef.current;
    if (!map) return;
    markersRef.current.forEach((m) => m.remove());
    markersRef.current = [];
    const index = indexRef.current;
    if (!index) {
      // An empty range: no pins, an empty strip — not stale leftovers.
      inViewRef.current = [];
      setInViewCount(0);
      setStrip([]);
      return;
    }
    const b = map.getBounds();
    const bbox: [number, number, number, number] = [
      b.getWest(),
      b.getSouth(),
      b.getEast(),
      b.getNorth(),
    ];
    const zoom = Math.round(map.getZoom());
    const clusters = index.getClusters(bbox, zoom);

    markersRef.current = clusters.slice(0, MAX_MARKERS).map((c) => {
      const [lon, lat] = (c.geometry as GeoJSON.Point).coordinates;
      const props = c.properties as ClusterProps;
      const isCluster = props.cluster === true;
      const count = isCluster ? props.point_count ?? 0 : 1;
      const coverId = isCluster
        ? (index.getLeaves(props.cluster_id!, 1)[0].properties as ClusterProps).id!
        : props.id!;

      const el = document.createElement("div");
      el.className = "geo-marker";
      const img = document.createElement("img");
      img.src = thumbUrl(coverId);
      img.draggable = false;
      img.onerror = () => el.classList.add("geo-noimg");
      el.appendChild(img);
      if (count > 1) {
        const badge = document.createElement("span");
        badge.className = "geo-count";
        badge.textContent = count > 999 ? `${Math.round(count / 1000)}k` : String(count);
        el.appendChild(badge);
      }
      el.addEventListener("click", (e) => {
        e.stopPropagation();
        if (isCluster) {
          const target = Math.min(index.getClusterExpansionZoom(props.cluster_id!), 17);
          map.easeTo({ center: [lon, lat], zoom: target, duration: 500 });
        } else {
          const i = inViewRef.current.findIndex((p) => p.id === props.id);
          if (i >= 0) setViewerIndex(i);
        }
      });
      return new maplibregl.Marker({ element: el }).setLngLat([lon, lat]).addTo(map);
    });

    const inView = shownRef.current
      .filter((p) => b.contains([p.lon, p.lat]))
      .sort((a, b2) => b2.ts - a.ts);
    inViewRef.current = inView;
    setInViewCount(inView.length);
    setStrip(inView.slice(0, MAX_STRIP));
  }, []);

  // Commit a year range: filter the full point set, rebuild the index, redraw.
  const applyRange = useCallback(
    (r: [number, number] | null) => {
      setRange(r);
      const pts =
        r == null
          ? pointsRef.current
          : pointsRef.current.filter((p) => {
              const y = new Date(p.ts * 1000).getFullYear();
              return y >= r[0] && y <= r[1];
            });
      shownRef.current = pts;
      setShownCount(pts.length);
      buildIndex(pts);
      refreshView();
    },
    [buildIndex, refreshView],
  );

  // Drag across the bars to select a range (a click is a one-year range). The
  // highlight follows the pointer live; the index rebuild waits for release.
  const yearAt = useCallback(
    (clientX: number): number => {
      const el = barsRef.current;
      if (!el || years.length === 0) return 0;
      const r = el.getBoundingClientRect();
      const f = Math.min(0.999, Math.max(0, (clientX - r.left) / r.width));
      return years[Math.floor(f * years.length)].y;
    },
    [years],
  );
  const onBarsPointerDown = (e: React.PointerEvent) => {
    e.preventDefault();
    const start = yearAt(e.clientX);
    setRange([start, start]);
    const span = (x: number): [number, number] => [Math.min(start, x), Math.max(start, x)];
    const move = (ev: PointerEvent) => setRange(span(yearAt(ev.clientX)));
    const up = (ev: PointerEvent) => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      const r = span(yearAt(ev.clientX));
      // Selecting everything is the same as selecting nothing.
      if (years.length > 0 && r[0] === years[0].y && r[1] === years[years.length - 1].y) {
        applyRange(null);
      } else {
        applyRange(r);
      }
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  useEffect(() => {
    let disposed = false;
    let map: maplibregl.Map | undefined;

    (async () => {
      const [size, points] = await Promise.all([
        basemapSize().catch(() => 0),
        getGeoPoints().catch(() => [] as GeoPoint[]),
      ]);
      if (disposed) return;
      pointsRef.current = points;
      setPointCount(points.length);
      if (size === 0 || !containerRef.current) {
        setStatus("nomap");
        return;
      }

      shownRef.current = points;
      setShownCount(points.length);
      buildIndex(points);
      // Photos per year → the histogram (only worth drawing across 2+ years).
      const counts = new Map<number, number>();
      for (const p of points) {
        const y = new Date(p.ts * 1000).getFullYear();
        counts.set(y, (counts.get(y) ?? 0) + 1);
      }
      if (counts.size >= 2) {
        const lo = Math.min(...counts.keys());
        const hi = Math.max(...counts.keys());
        const ys: { y: number; n: number }[] = [];
        for (let y = lo; y <= hi; y++) ys.push({ y, n: counts.get(y) ?? 0 });
        setYears(ys);
      }

      ensureBasemapProtocol();
      map = new maplibregl.Map({
        container: containerRef.current,
        style: basemapStyle(basemapFlavor()),
        center: [0, 20],
        zoom: 1.4,
        attributionControl: false,
      });
      map.addControl(new maplibregl.AttributionControl({ compact: true }));
      map.addControl(new maplibregl.NavigationControl({ showCompass: false }), "top-right");
      mapRef.current = map;
      map.on("load", () => {
        if (disposed) return;
        setStatus("ready");
        refreshView();
      });
      map.on("moveend", refreshView);
      map.on("error", (e) => {
        const msg = e?.error?.message ?? String(e?.error ?? "unknown map error");
        console.error("map error:", e);
        setMapError((cur) => cur ?? msg);
      });
    })();

    return () => {
      disposed = true;
      markersRef.current.forEach((m) => m.remove());
      markersRef.current = [];
      map?.remove();
      mapRef.current = null;
    };
  }, [refreshView, buildIndex]);

  return (
    <div className="places">
      <div ref={containerRef} className="places-map" />

      {status === "loading" && (
        <div className="places-note">
          <span className="spinner" />
        </div>
      )}

      {mapError && (
        <div className="places-note places-error">
          <p>The map hit an error.</p>
          <p className="muted">{mapError}</p>
          <button className="ghost-btn" onClick={() => setMapError(null)}>
            Dismiss
          </button>
        </div>
      )}

      {status === "nomap" && (
        <div className="places-note">
          <p>The offline map isn't bundled in this build.</p>
          <p className="muted">
            Run <code>scripts/fetch-basemap.sh</code> and rebuild — Solar's map works entirely
            offline, so the world data ships inside the app.
          </p>
        </div>
      )}

      {status === "ready" && pointCount === 0 && (
        <div className="places-note">
          <p>No located photos yet.</p>
          <p className="muted">
            Locations come from your photos' own metadata as they're indexed — and for photos
            kept in the cloud, after they download. Check back as the library fills in.
          </p>
        </div>
      )}

      {/* Bottom overlays stack in one column — histogram above filmstrip —
          so they size independently and can never overlap. */}
      <div className="geo-bottom">
      {status === "ready" && years.length >= 2 && (
        <div className="geo-histo">
          <div className="gh-bars" ref={barsRef} onPointerDown={onBarsPointerDown}>
            {years.map(({ y, n }) => {
              const max = Math.max(1, ...years.map((x) => x.n));
              const sel = range == null || (y >= range[0] && y <= range[1]);
              return (
                <i
                  key={y}
                  className={sel ? "sel" : ""}
                  style={{ height: `${Math.max(8, (n / max) * 100)}%` }}
                  title={`${y} · ${n.toLocaleString()}`}
                />
              );
            })}
          </div>
          <div className="gh-lbl">
            <span>{years[0].y}</span>
            <b>
              {range == null
                ? "All years"
                : range[0] === range[1]
                  ? String(range[0])
                  : `${range[0]} – ${range[1]}`}
              {" · "}
              {shownCount.toLocaleString()} {shownCount === 1 ? "photo" : "photos"}
              {range != null && (
                <button className="gh-clear" title="Show all years" onClick={() => applyRange(null)}>
                  ✕
                </button>
              )}
            </b>
            <span>{years[years.length - 1].y}</span>
          </div>
        </div>
      )}

      {status === "ready" && inViewCount > 0 && (
        <div className="geo-strip-wrap">
          <div className="geo-strip-count">
            {inViewCount.toLocaleString()} {inViewCount === 1 ? "photo" : "photos"} here
          </div>
          <div className="geo-strip">
            {strip.map((p, i) => (
              <img
                key={p.id}
                className="geo-strip-thumb"
                src={thumbUrl(p.id)}
                loading="lazy"
                draggable={false}
                onClick={() => setViewerIndex(i)}
              />
            ))}
          </div>
        </div>
      )}
      </div>

      {viewerIndex !== null && (
        <Lightbox
          index={viewerIndex}
          total={inViewRef.current.length}
          resolveId={resolveId}
          onClose={() => setViewerIndex(null)}
        />
      )}
    </div>
  );
}
