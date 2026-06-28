import { useCallback, useEffect, useState } from "react";
import PhotoGrid from "./PhotoGrid";
import {
  getLibraryStats,
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
    onThumbReady(() => setReady((r) => r + 1)).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, []);

  const handlePick = useCallback(async () => {
    const path = await pickFolder();
    if (!path) return;
    setFolder(path);
    setScanning(true);
    setReady(0);
    try {
      const newTotal = await scanFolder(path);
      setTotal(newTotal);
    } finally {
      setScanning(false);
    }
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
        <PhotoGrid total={total} />
      ) : (
        <div className="empty">
          <p>Pick a folder of photos to begin.</p>
          <p className="muted">
            JPEG, HEIC, PNG and WebP are indexed recursively. Thumbnails are
            generated in the background and cached to disk.
          </p>
        </div>
      )}
    </div>
  );
}

export default App;
