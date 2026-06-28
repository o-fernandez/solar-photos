import { useCallback, useEffect, useState } from "react";
import PhotoGrid from "./PhotoGrid";
import {
  getLibraryStats,
  onScanProgress,
  onThumbReady,
  pickFolder,
  scanFolder,
} from "./api";
import "./App.css";

function App() {
  const [total, setTotal] = useState(0);
  const [ready, setReady] = useState(0);
  const [scanning, setScanning] = useState(false);
  const [folder, setFolder] = useState<string | null>(null);

  // Cold start (Principle 4): on launch we read the already-indexed counts from
  // the DB and render the grid immediately. No rescan, no "loading" wall.
  useEffect(() => {
    getLibraryStats()
      .then((s) => {
        setTotal(s.total);
        setReady(s.ready);
      })
      .catch(() => {});
  }, []);

  // Keep the "ready" counter live as thumbnails stream in. This is a count only;
  // it never touches the grid's scroll position.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onThumbReady((d) => {
      if (d.ok) setReady((r) => r + 1);
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, []);

  // The scan streams its progress: `total` grows per batch so the grid fills in
  // live, and `scanning` clears when the walk finishes. The UI never blocks
  // waiting for a huge or cloud-backed folder (Principle 1).
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onScanProgress((p) => {
      setTotal(p.found);
      if (p.done) {
        setScanning(false);
        // Re-sync the authoritative counts once the walk is complete.
        getLibraryStats()
          .then((s) => {
            setTotal(s.total);
            setReady(s.ready);
          })
          .catch(() => {});
      }
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, []);

  const handlePick = useCallback(async () => {
    const path = await pickFolder();
    if (!path) return;
    setFolder(path);
    setScanning(true);
    // Fire and forget — returns immediately; progress arrives via events.
    scanFolder(path).catch(() => setScanning(false));
  }, []);

  const pct = total > 0 ? Math.round((ready / total) * 100) : 0;

  return (
    <div className="app">
      <header className="toolbar">
        <button className="pick-btn" onClick={handlePick} disabled={scanning}>
          {scanning ? "Scanning…" : "Pick folder"}
        </button>
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
        {folder && <div className="folder" title={folder}>{folder}</div>}
      </header>

      {total > 0 ? (
        // Remount on a new folder so the grid's caches reset cleanly. While a
        // scan runs, show discovery order (append-only, no reflow); once it
        // finishes, snap to the newest-first timeline.
        <PhotoGrid key={folder ?? "library"} total={total} byDate={!scanning} />
      ) : (
        <div className="empty">
          <p>Pick a folder of photos to begin.</p>
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
