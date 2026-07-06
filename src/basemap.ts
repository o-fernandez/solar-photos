// Shared setup for the offline basemap, used by both the Places tab and the
// Home screen's map preview. The bundled PMTiles archive is read by byte range
// over IPC (readBasemapRange) through a custom maplibre protocol — no tile
// server is ever contacted.

import maplibregl from "maplibre-gl";
import { PMTiles, Protocol, type RangeResponse, type Source } from "pmtiles";
import { layers, namedFlavor } from "@protomaps/basemaps";
import { readBasemapRange } from "./api";

/** PMTiles source over the bundled archive: byte ranges via Tauri IPC. */
class BundledBasemap implements Source {
  getKey() {
    return "bundled-world";
  }
  async getBytes(offset: number, length: number): Promise<RangeResponse> {
    return { data: await readBasemapRange(offset, length) };
  }
}

// Register the pmtiles:// protocol once per app run (idempotent — safe to call
// from every view that mounts a map).
let protocolRegistered = false;
export function ensureBasemapProtocol() {
  if (protocolRegistered) return;
  const protocol = new Protocol();
  protocol.add(new PMTiles(new BundledBasemap()));
  maplibregl.addProtocol("pmtiles", protocol.tile);
  protocolRegistered = true;
}

/** The current OS-theme flavor for the basemap style. */
export function basemapFlavor(): "light" | "dark" {
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

/** A MapLibre style over the bundled basemap. `globe` spins at world level and
 *  flattens as you zoom; the Home preview passes false for a flat mini-map. */
export function basemapStyle(flavor: "light" | "dark", globe = true): maplibregl.StyleSpecification {
  return {
    version: 8,
    projection: globe ? { type: "globe" } : undefined,
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
  } as maplibregl.StyleSpecification;
}
