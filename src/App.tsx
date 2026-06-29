import { useCallback, useEffect, useState } from "react";
import PhotoGrid from "./PhotoGrid";
import People from "./People";
import {
  addFolder,
  getFaceProgress,
  getLibraryStats,
  listRoots,
  onFaceProgress,
  onScanProgress,
  onThumbReady,
  pickFolder,
  removeFolder,
  rescan,
} from "./api";
import "./App.css";

function App() {
  const [total, setTotal] = useState(0);
  const [ready, setReady] = useState(0);
  const [scanning, setScanning] = useState(false);
  const [rescanning, setRescanning] = useState(false);
  const [roots, setRoots] = useState<string[]>([]);
  const [showRoots, setShowRoots] = useState(false);
  // Bumped whenever the library changes on disk (add / rescan / remove) so the
  // grid drops its cached index→photo mapping and refetches the truth.
  const [refreshKey, setRefreshKey] = useState(0);
  const [faceScanned, setFaceScanned] = useState(0);
  const [faceEligible, setFaceEligible] = useState(0);
  const [view, setView] = useState<"timeline" | "people">("timeline");

  const refreshRoots = useCallback(() => {
    listRoots().then(setRoots).catch(() => {});
  }, []);

  const refreshStats = useCallback(() => {
    getLibraryStats()
      .then((s) => {
        setTotal(s.total);
        setReady(s.ready);
      })
      .catch(() => {});
  }, []);

  // Cold start (Principle 4): render from the already-indexed counts immediately;
  // the backend's auto-rescan reconciles with disk in the background.
  useEffect(() => {
    refreshStats();
    refreshRoots();
    getFaceProgress()
      .then((p) => {
        setFaceScanned(p.scanned);
        setFaceEligible(p.eligible);
      })
      .catch(() => {});
  }, [refreshStats, refreshRoots]);

  // Background face-sweep progress (non-intrusive; full People view comes later).
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onFaceProgress((p) => {
      setFaceScanned(p.scanned);
      setFaceEligible(p.eligible);
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, []);

  // Keep the "ready" counter live as thumbnails stream in (successes only).
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onThumbReady((d) => {
      if (d.ok) setReady((r) => r + 1);
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, []);

  // Scan/rescan progress: grow the count live; on done, settle the truth and
  // tell the grid to refresh.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onScanProgress((p) => {
      setTotal(p.found);
      if (p.done) {
        setScanning(false);
        setRescanning(false);
        refreshStats();
        refreshRoots();
        setRefreshKey((k) => k + 1);
      }
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, [refreshStats, refreshRoots]);

  const handleAdd = useCallback(async () => {
    const path = await pickFolder();
    if (!path) return;
    setScanning(true);
    addFolder(path)
      .then(refreshRoots)
      .catch(() => setScanning(false));
  }, [refreshRoots]);

  const handleRescan = useCallback(() => {
    setRescanning(true);
    rescan().catch(() => setRescanning(false));
  }, []);

  const handleRemove = useCallback((path: string) => {
    removeFolder(path).catch(() => {});
    // The new total + refresh arrive via the scan-progress "done" event.
  }, []);

  const pct = total > 0 ? Math.round((ready / total) * 100) : 0;
  const busy = scanning || rescanning;

  return (
    <div className="app">
      <header className="toolbar">
        <button className="pick-btn" onClick={handleAdd} disabled={busy}>
          {scanning ? "Scanning…" : "Add folder"}
        </button>
        {total > 0 && (
          <button className="ghost-btn" onClick={handleRescan} disabled={busy}>
            {rescanning ? "Rescanning…" : "Rescan"}
          </button>
        )}

        {total > 0 && (
          <nav className="view-nav">
            <button
              className={view === "timeline" ? "on" : ""}
              onClick={() => setView("timeline")}
            >
              Timeline
            </button>
            <button
              className={view === "people" ? "on" : ""}
              onClick={() => setView("people")}
            >
              People
            </button>
          </nav>
        )}

        <div className="status">
          {total > 0 ? (
            <>
              <span className="count">{total.toLocaleString()} photos</span>
              {ready < total ? (
                <span className="progress">
                  · {ready.toLocaleString()} thumbnails ready ({pct}%)
                </span>
              ) : (
                <span className="progress done">· all thumbnails cached</span>
              )}
            </>
          ) : (
            <span className="count muted">No folder indexed yet</span>
          )}
        </div>

        {faceEligible > 0 && faceScanned < faceEligible && (
          <span className="faces-pill">
            Finding faces… {Math.round((faceScanned / faceEligible) * 100)}%
          </span>
        )}

        {roots.length > 0 && (
          <div className="roots">
            <button className="ghost-btn" onClick={() => setShowRoots((s) => !s)}>
              {roots.length} {roots.length === 1 ? "folder" : "folders"} ▾
            </button>
            {showRoots && (
              <div className="roots-menu">
                {roots.map((r) => (
                  <div className="roots-item" key={r}>
                    <span className="roots-path" title={r}>
                      {r}
                    </span>
                    <button
                      className="roots-remove"
                      aria-label={`Remove ${r}`}
                      onClick={() => handleRemove(r)}
                    >
                      ✕
                    </button>
                  </div>
                ))}
              </div>
            )}
          </div>
        )}
      </header>

      {total > 0 ? (
        view === "people" ? (
          <People />
        ) : (
          // Discovery order while a scan runs (append-only, no reflow); snap to
          // the newest-first timeline otherwise. refreshKey forces a refetch
          // when the library changes on disk.
          <PhotoGrid total={total} byDate={!scanning} refreshKey={refreshKey} />
        )
      ) : (
        <div className="empty">
          <p>Add a folder of photos to begin.</p>
          <p className="muted">
            JPEG, HEIC, PNG and WebP are indexed recursively. Thumbnails are
            generated in the background and cached to disk. Photos kept in the
            cloud download on demand as you browse.
          </p>
        </div>
      )}
    </div>
  );
}

export default App;
