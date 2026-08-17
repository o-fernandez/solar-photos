// The Home screen: a discovery feed that greets you with a bit of everything —
// on this day, your people, your places, favorites, and the newest photos — so
// the whole library is one glance away instead of split behind tabs. Each shelf
// links into its full view (Timeline / People / Places / Favorites).
//
// Everything here is local and already computed: no cloud "memories," no
// generated trips — just honest slices of your own library.

import { useCallback, useEffect, useRef, useState } from "react";
import maplibregl from "maplibre-gl";
import Lightbox from "./Lightbox";
import {
  faceCropUrl,
  getClusters,
  getDuplicateReport,
  getGeoPoints,
  getOnThisDay,
  getPhotosRange,
  thumbUrl,
  type Cluster,
  type GeoPoint,
  type PhotoRow,
} from "./api";
import { basemapFlavor, basemapStyle, ensureBasemapProtocol } from "./basemap";
import { fmtBytes } from "./format";

const SHELF = 24; // photos fetched per row shelf
const PEOPLE = 12; // face tiles on the people shelf

type View = "timeline" | "favorites" | "people" | "places" | "home" | "hidden" | "duplicates";

function monthDayYear(ts: number): string {
  return new Date(ts * 1000).toLocaleDateString(undefined, {
    month: "long",
    day: "numeric",
    year: "numeric",
  });
}

function greeting(): string {
  const h = new Date().getHours();
  if (h < 12) return "Good morning";
  if (h < 18) return "Good afternoon";
  return "Good evening";
}

export default function Home({
  onNavigate,
  onOpenPerson,
}: {
  onNavigate: (view: View) => void;
  onOpenPerson: (cluster: Cluster) => void;
}) {
  const [onThisDay, setOnThisDay] = useState<PhotoRow[]>([]);
  const [people, setPeople] = useState<Cluster[]>([]);
  const [favorites, setFavorites] = useState<PhotoRow[]>([]);
  const [recent, setRecent] = useState<PhotoRow[]>([]);
  const [geoCount, setGeoCount] = useState(0);
  // The duplicates shelf: how many groups wait, what they waste, one cover.
  const [dupes, setDupes] = useState<{ groups: number; wasted: number; coverId: number | null } | null>(null);

  // A Lightbox scoped to whichever shelf was clicked.
  const [viewerPhotos, setViewerPhotos] = useState<PhotoRow[]>([]);
  const [viewerIndex, setViewerIndex] = useState<number | null>(null);
  const viewerRef = useRef<PhotoRow[]>([]);
  viewerRef.current = viewerPhotos;
  const resolveId = useCallback(
    (i: number): Promise<number | null> => Promise.resolve(viewerRef.current[i]?.id ?? null),
    [],
  );
  const openViewer = (photos: PhotoRow[], i: number) => {
    setViewerPhotos(photos);
    setViewerIndex(i);
  };

  const load = useCallback(() => {
    getOnThisDay().then(setOnThisDay).catch(() => {});
    getClusters()
      .then((cs) => setPeople(cs.filter((c) => c.name).slice(0, PEOPLE)))
      .catch(() => {});
    getPhotosRange(0, SHELF, true, "favorites").then(setFavorites).catch(() => {});
    getPhotosRange(0, SHELF, true, "visible").then(setRecent).catch(() => {});
    getGeoPoints().then((p) => setGeoCount(p.length)).catch(() => {});
    getDuplicateReport()
      .then((r) =>
        setDupes({
          groups: r.groups.length,
          wasted: r.groups.reduce((n, g) => n + g.wasted_bytes, 0),
          coverId: r.groups[0]?.copies[0]?.id ?? null,
        }),
      )
      .catch(() => {});
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  // --- On this day: one cover per past year that has photos on this date, newest
  // year first. Each cover opens that year's cluster in the viewer. ---
  const nowYear = new Date().getFullYear();
  const byYear = new Map<number, PhotoRow[]>();
  for (const p of onThisDay) {
    const y = new Date(p.ts * 1000).getFullYear();
    if (y >= nowYear) continue; // "on this day" is about years gone by
    (byYear.get(y) ?? byYear.set(y, []).get(y)!).push(p);
  }
  const otdYears = [...byYear.entries()].sort((a, b) => b[0] - a[0]);
  const otdTotal = otdYears.reduce((n, [, ps]) => n + ps.length, 0);

  return (
    <div className="home-scroll">
      <div className="home-greet">
        {greeting()}
        <span className="home-greet-sub"> — a look back through your library</span>
      </div>

      {otdYears.length > 0 && (
        <Shelf
          title="On this day"
          sub={`${otdYears.length} ${otdYears.length === 1 ? "year" : "years"} · ${otdTotal.toLocaleString()} ${otdTotal === 1 ? "photo" : "photos"}`}
        >
          <div className="home-row otd-row">
            {otdYears.map(([year, photos]) => {
              const ago = nowYear - year;
              return (
                <button
                  key={year}
                  className="otd-cover"
                  onClick={() => openViewer(photos, 0)}
                  title={`${monthDayYear(photos[0].ts)} — ${photos.length} ${photos.length === 1 ? "photo" : "photos"}`}
                >
                  <div className="otd-cover-img">
                    <img src={thumbUrl(photos[0].id)} loading="lazy" draggable={false} />
                    <span className="otd-badge">{ago} {ago === 1 ? "year" : "years"} ago</span>
                    {photos.length > 1 && <span className="otd-stack" aria-hidden="true" />}
                  </div>
                  <div className="otd-cap">
                    {year} <span className="otd-cnt">· {photos.length.toLocaleString()}</span>
                  </div>
                </button>
              );
            })}
          </div>
        </Shelf>
      )}

      {people.length > 0 && (
        <Shelf title="Your people" action="See all" onAction={() => onNavigate("people")}>
          <div className="home-row">
            {people.map((c) => (
              <button key={c.cluster_id} className="home-person" onClick={() => onOpenPerson(c)}>
                <img className="home-face" src={faceCropUrl(c.cover_face_id)} alt="" draggable={false} />
                <span className="home-person-name">{c.name}</span>
                <span className="home-person-count">{c.count.toLocaleString()}</span>
              </button>
            ))}
          </div>
        </Shelf>
      )}

      {geoCount > 0 && (
        <Shelf title="Your places" action="Open map" onAction={() => onNavigate("places")}>
          <PlacesPreview count={geoCount} onOpen={() => onNavigate("places")} />
        </Shelf>
      )}

      {dupes != null && dupes.groups > 0 && (
        <Shelf title="Duplicates" action="Review" onAction={() => onNavigate("duplicates")}>
          <button className="home-dupes" onClick={() => onNavigate("duplicates")}>
            <span className="hd-stack" aria-hidden="true">
              {dupes.coverId != null && (
                <>
                  <img src={thumbUrl(dupes.coverId)} alt="" draggable={false} />
                  <img src={thumbUrl(dupes.coverId)} alt="" draggable={false} />
                </>
              )}
            </span>
            <span className="hd-text">
              <b>
                {dupes.groups.toLocaleString()} exact{" "}
                {dupes.groups === 1 ? "duplicate" : "duplicates"}
              </b>
              {fmtBytes(dupes.wasted)} of repeats across your folders — hide the extras in a
              quick pass
            </span>
          </button>
        </Shelf>
      )}

      {favorites.length > 0 && (
        <Shelf title="Favorites" action="See all" onAction={() => onNavigate("favorites")}>
          <div className="home-row">
            {favorites.map((p, i) => (
              <div key={p.id} className="home-thumb-wrap" onClick={() => openViewer(favorites, i)}>
                <img className="home-thumb" src={thumbUrl(p.id)} loading="lazy" draggable={false} />
                <span className="home-heart" aria-hidden="true">&#9829;</span>
              </div>
            ))}
          </div>
        </Shelf>
      )}

      {recent.length > 0 && (
        <Shelf title="Recently added" action="Timeline" onAction={() => onNavigate("timeline")}>
          <div className="home-row">
            {recent.map((p, i) => (
              <img
                key={p.id}
                className="home-thumb"
                src={thumbUrl(p.id)}
                loading="lazy"
                draggable={false}
                onClick={() => openViewer(recent, i)}
              />
            ))}
          </div>
        </Shelf>
      )}

      {viewerIndex !== null && (
        <Lightbox
          index={viewerIndex}
          total={viewerPhotos.length}
          resolveId={resolveId}
          onClose={() => setViewerIndex(null)}
          onCorrection={load}
        />
      )}
    </div>
  );
}

function Shelf({
  title,
  sub,
  action,
  onAction,
  children,
}: {
  title: string;
  sub?: string;
  action?: string;
  onAction?: () => void;
  children: React.ReactNode;
}) {
  return (
    <section className="home-shelf">
      <div className="home-shelf-head">
        <span className="home-shelf-title">{title}</span>
        {sub && <span className="home-shelf-sub">{sub}</span>}
        {action && (
          <button className="home-shelf-action" onClick={onAction}>
            {action}
          </button>
        )}
      </div>
      {children}
    </section>
  );
}

// A live, non-interactive mini-map of everywhere you've been — the same bundled
// offline basemap as the Places tab, fit to your photos, click to open the globe.
function PlacesPreview({ count, onOpen }: { count: number; onOpen: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!ref.current) return;
    let map: maplibregl.Map | undefined;
    let disposed = false;
    ensureBasemapProtocol();
    map = new maplibregl.Map({
      container: ref.current,
      style: basemapStyle(basemapFlavor(), false),
      center: [0, 20],
      zoom: 0.6,
      interactive: false,
      attributionControl: false,
    });
    map.on("load", () => {
      if (disposed || !map) return;
      getGeoPoints()
        .then((points) => {
          if (disposed || !map || points.length === 0) return;
          map.addSource("pts", {
            type: "geojson",
            data: {
              type: "FeatureCollection",
              features: points.map((p: GeoPoint) => ({
                type: "Feature",
                geometry: { type: "Point", coordinates: [p.lon, p.lat] },
                properties: {},
              })),
            },
          });
          map.addLayer({
            type: "circle",
            id: "pts",
            source: "pts",
            paint: {
              "circle-radius": 3,
              "circle-color": "#f5a623",
              "circle-opacity": 0.85,
              "circle-stroke-width": 0.5,
              "circle-stroke-color": "#fff",
            },
          });
          // Frame everywhere you've been.
          // Do not spread the full library into Math.min/Math.max: WebKit's
          // function-argument limit is lower than Solar's 100k-photo target.
          const bounds = points.reduce(
            (b, p) => ({
              west: Math.min(b.west, p.lon),
              south: Math.min(b.south, p.lat),
              east: Math.max(b.east, p.lon),
              north: Math.max(b.north, p.lat),
            }),
            { west: Infinity, south: Infinity, east: -Infinity, north: -Infinity },
          );
          map.fitBounds(
            [[bounds.west, bounds.south], [bounds.east, bounds.north]],
            { padding: 30, duration: 0, maxZoom: 6 },
          );
        })
        .catch(() => {});
    });
    return () => {
      disposed = true;
      map?.remove();
    };
  }, []);

  return (
    <button className="home-map-card" onClick={onOpen} aria-label="Open the map">
      <div ref={ref} className="home-map" />
      <span className="home-map-label">{count.toLocaleString()} located photos</span>
    </button>
  );
}
