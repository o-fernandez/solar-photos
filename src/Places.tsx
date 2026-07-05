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
import { PMTiles, Protocol, type RangeResponse, type Source } from "pmtiles";
import { layers, namedFlavor } from "@protomaps/basemaps";
import Supercluster from "supercluster";
import Lightbox from "./Lightbox";
import { basemapSize, getGeoPoints, readBasemapRange, thumbUrl, type GeoPoint } from "./api";

// How many markers we render per view (clusters keep it low anyway) and how
// many thumbs the filmstrip mounts (each is an <img> request — P6).
const MAX_MARKERS = 150;
const MAX_STRIP = 200;

/** PMTiles source over the bundled archive: byte ranges via Tauri IPC. */
class BundledBasemap implements Source {
  getKey() {
    return "bundled-world";
  }
  async getBytes(offset: number, length: number): Promise<RangeResponse> {
    return { data: await readBasemapRange(offset, length) };
  }
}

// Register the pmtiles:// protocol once per app run.
let protocolRegistered = false;
function ensureProtocol() {
  if (protocolRegistered) return;
  const protocol = new Protocol();
  protocol.add(new PMTiles(new BundledBasemap()));
  maplibregl.addProtocol("pmtiles", protocol.tile);
  protocolRegistered = true;
}

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

  const resolveId = useCallback(
    (i: number): Promise<number | null> => Promise.resolve(inViewRef.current[i]?.id ?? null),
    [],
  );

  // Recompute markers + filmstrip for the current viewport. Reads refs only, so
  // the single moveend listener never goes stale.
  const refreshView = useCallback(() => {
    const map = mapRef.current;
    const index = indexRef.current;
    if (!map || !index) return;
    const b = map.getBounds();
    const bbox: [number, number, number, number] = [
      b.getWest(),
      b.getSouth(),
      b.getEast(),
      b.getNorth(),
    ];
    const zoom = Math.round(map.getZoom());
    const clusters = index.getClusters(bbox, zoom);

    markersRef.current.forEach((m) => m.remove());
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

    const inView = pointsRef.current
      .filter((p) => b.contains([p.lon, p.lat]))
      .sort((a, b2) => b2.ts - a.ts);
    inViewRef.current = inView;
    setInViewCount(inView.length);
    setStrip(inView.slice(0, MAX_STRIP));
  }, []);

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

      // Self-check the IPC byte-range transport before the map depends on it:
      // the first 16KB must start with the PMTiles magic ("PM").
      try {
        const head = new DataView(await readBasemapRange(0, 16384));
        if (head.getUint16(0, true) !== 0x4d50) {
          setMapError(`basemap transport corrupt (magic ${head.getUint16(0, true)})`);
        }
      } catch (e) {
        setMapError(`basemap transport failed: ${e}`);
      }

      if (points.length > 0) {
        const index = new Supercluster<ClusterProps>({ radius: 64, maxZoom: 17, minPoints: 2 });
        index.load(
          points.map((p) => ({
            type: "Feature" as const,
            geometry: { type: "Point" as const, coordinates: [p.lon, p.lat] },
            properties: { id: p.id, ts: p.ts },
          })),
        );
        indexRef.current = index;
      }

      ensureProtocol();
      const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
      const flavor = dark ? "dark" : "light";
      map = new maplibregl.Map({
        container: containerRef.current,
        style: {
          version: 8,
          projection: { type: "globe" },
          glyphs: `${location.origin}/basemap-assets/fonts/{fontstack}/{range}.pbf`,
          sprite: `${location.origin}/basemap-assets/sprites/v4/${flavor}`,
          sources: {
            protomaps: {
              type: "vector",
              url: "pmtiles://bundled-world",
              attribution: "© OpenStreetMap",
            },
          },
          layers: layers("protomaps", namedFlavor(flavor), { lang: "en" }),
        },
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
  }, [refreshView]);

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
