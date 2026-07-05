import { useCallback, useEffect, useRef, useState } from "react";
import PhotoGrid from "./PhotoGrid";
import People from "./People";
import Places from "./Places";
import {
  addFolder,
  faceCropUrl,
  getClusters,
  getFaceProgress,
  getLibraryStats,
  listRoots,
  onFaceProgress,
  onScanProgress,
  onThumbReady,
  pickFolder,
  removeFolder,
  rescan,
  resetFaceDecisions,
  type Cluster,
} from "./api";
import "./App.css";

// How often (ms) to re-check clusters for a freshly-qualified new person while
// the sweep runs, and how long the resulting toast lingers before fading.
const NEW_PERSON_CHECK_MS = 20_000;
const TOAST_LINGER_MS = 12_000;
// A new person must reach this many photos before we nudge — matches People's
// mid-sweep floor, so we only celebrate clusters we're confident are real.
const NEW_PERSON_FLOOR = 8;
// During a fast sweep many same-person fragments cross the floor in a row; nudging
// on each one is noise (and pushes premature labeling). Celebrate at most one new
// person per window so the toast stays an occasional delight, not a task queue.
const TOAST_COOLDOWN_MS = 45_000;

function App() {
  const [total, setTotal] = useState(0);
  const [ready, setReady] = useState(0);
  const [scanning, setScanning] = useState(false);
  const [rescanning, setRescanning] = useState(false);
  const [roots, setRoots] = useState<string[]>([]);
  // Bumped whenever the library changes on disk (add / rescan / remove) so the
  // grid drops its cached index→photo mapping and refetches the truth.
  const [refreshKey, setRefreshKey] = useState(0);
  const [faceScanned, setFaceScanned] = useState(0);
  const [faceEligible, setFaceEligible] = useState(0);
  const [view, setView] = useState<"timeline" | "people" | "places">("timeline");
  // Settings menu (Add folder / Rescan / folders), tucked behind the gear.
  const [showSettings, setShowSettings] = useState(false);
  // Two-click guard on the destructive "start people over" reset.
  const [confirmReset, setConfirmReset] = useState(false);
  // Two-click guard on removing a folder (the ✕ sits right next to each path —
  // one stray click shouldn't drop thousands of photos from the index).
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null);
  // Where the pre-reset database backup landed — shown once so the safety net is
  // felt, not just logged to a console nobody has open.
  const [resetNote, setResetNote] = useState<string | null>(null);
  // The "new friend" nudge: the freshly-qualified person to celebrate, plus the
  // cluster People should open the name field for when you act on it.
  const [newPerson, setNewPerson] = useState<Cluster | null>(null);
  const [focusClusterId, setFocusClusterId] = useState<number | null>(null);
  // Cluster ids we've already announced (or seeded on first load), so a person is
  // nudged exactly once and the existing library doesn't fire a burst at startup.
  const announcedRef = useRef<Set<number>>(new Set());
  const seededRef = useRef(false);
  const lastPeopleCheckRef = useRef(0);
  const lastToastRef = useRef(0);

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

  // Background face-sweep progress feeds the hairline indicator, and — throttled —
  // watches for a newly-qualified person to nudge. The first check seeds the
  // "already seen" set silently so the existing library doesn't toast on launch;
  // afterward, any unnamed cluster that crosses the floor gets celebrated once.
  // The counts land in state at most ~2×/second: the raw events arrive per photo,
  // and re-rendering the app per event starves hover/paint under a long backfill.
  const facePendingRef = useRef<{ scanned: number; eligible: number } | null>(null);
  const faceFlushTimer = useRef<number | undefined>(undefined);
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onFaceProgress((p) => {
      facePendingRef.current = { scanned: p.scanned, eligible: p.eligible };
      if (faceFlushTimer.current == null) {
        faceFlushTimer.current = window.setTimeout(() => {
          faceFlushTimer.current = undefined;
          const latest = facePendingRef.current;
          if (latest) {
            setFaceScanned(latest.scanned);
            setFaceEligible(latest.eligible);
          }
        }, 500);
      }
      const now = Date.now();
      if (now - lastPeopleCheckRef.current < NEW_PERSON_CHECK_MS) return;
      lastPeopleCheckRef.current = now;
      getClusters()
        .then((cs) => {
          const seen = announcedRef.current;
          const candidates = cs.filter((c) => c.name == null && c.count >= NEW_PERSON_FLOOR);
          if (!seededRef.current) {
            candidates.forEach((c) => seen.add(c.cluster_id));
            seededRef.current = true;
            return;
          }
          const fresh = candidates.filter((c) => !seen.has(c.cluster_id));
          fresh.forEach((c) => seen.add(c.cluster_id));
          if (fresh.length > 0 && now - lastToastRef.current >= TOAST_COOLDOWN_MS) {
            lastToastRef.current = now;
            setNewPerson(fresh.reduce((a, b) => (b.count > a.count ? b : a)));
          }
        })
        .catch(() => {});
    }).then((fn) => {
      unlisten = fn;
    });
    return () => {
      unlisten?.();
      if (faceFlushTimer.current != null) window.clearTimeout(faceFlushTimer.current);
    };
  }, []);

  // The nudge lingers, then fades on its own (a new one replaces it immediately).
  useEffect(() => {
    if (!newPerson) return;
    const t = setTimeout(() => setNewPerson(null), TOAST_LINGER_MS);
    return () => clearTimeout(t);
  }, [newPerson]);

  // Keep the "ready" counter live as thumbnails stream in (successes only).
  // Coalesced: a cloud backfill streams these for hours, and one state update per
  // event re-rendered the whole app per thumbnail (the hover-lag bug).
  const readyPendingRef = useRef(0);
  const readyFlushTimer = useRef<number | undefined>(undefined);
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onThumbReady((d) => {
      if (!d.ok) return;
      readyPendingRef.current += 1;
      if (readyFlushTimer.current == null) {
        readyFlushTimer.current = window.setTimeout(() => {
          readyFlushTimer.current = undefined;
          const n = readyPendingRef.current;
          readyPendingRef.current = 0;
          setReady((r) => r + n);
        }, 400);
      }
    }).then((fn) => {
      unlisten = fn;
    });
    return () => {
      unlisten?.();
      if (readyFlushTimer.current != null) window.clearTimeout(readyFlushTimer.current);
    };
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
    setConfirmRemove(null);
    removeFolder(path).catch(() => {});
    // The new total + refresh arrive via the scan-progress "done" event.
  }, []);

  // "Start people over": clear all names/groups/decisions (keeping detected faces) and
  // re-cluster from scratch. Backs up the DB first. A background re-cluster follows;
  // the People view refreshes itself when it lands.
  const handleResetPeople = useCallback(() => {
    setConfirmReset(false);
    setShowSettings(false);
    resetFaceDecisions()
      .then((backup) => setResetNote(backup))
      .catch(() => {});
  }, []);

  // The backup-path note lingers, then fades (dismissable meanwhile).
  useEffect(() => {
    if (!resetNote) return;
    const t = setTimeout(() => setResetNote(null), 15_000);
    return () => clearTimeout(t);
  }, [resetNote]);

  // Stable prop for the memoized People — an inline lambda would defeat the memo.
  const handleFocusConsumed = useCallback(() => setFocusClusterId(null), []);

  // Jump to the new person and open their name field straight away.
  const nameNewPerson = useCallback(() => {
    setNewPerson((p) => {
      if (p) {
        setFocusClusterId(p.cluster_id);
        setView("people");
      }
      return null;
    });
  }, []);

  const busy = scanning || rescanning;

  // One indicator for all background work: thumbnails first (the grid needs them),
  // then face-scanning, then nothing. Drives the single hairline under the bar.
  // The phase + counts live in the hover chip, so the line itself stays one calm,
  // warm color (the meaning comes from the label, not from a color the user has
  // to decode).
  const activity =
    total > 0 && ready < total
      ? { frac: ready / total, phase: "Preparing photos", done: ready, of: total }
      : faceEligible > 0 && faceScanned < faceEligible
        ? { frac: faceScanned / faceEligible, phase: "Finding people", done: faceScanned, of: faceEligible }
        : null;
  const activityLabel = activity
    ? `${activity.phase} · ${activity.done.toLocaleString()} of ${activity.of.toLocaleString()} · ${Math.round(activity.frac * 100)}%`
    : "";

  // When a phase starts or changes, show the hover chip on its own for a few
  // seconds — a 2px line whose meaning lives behind an undiscovered hover never
  // tells a new user what the app is doing.
  const phase = activity?.phase ?? null;
  const prevPhaseRef = useRef<string | null>(null);
  const [phaseFlash, setPhaseFlash] = useState(false);
  useEffect(() => {
    if (phase && phase !== prevPhaseRef.current) {
      prevPhaseRef.current = phase;
      setPhaseFlash(true);
      const t = setTimeout(() => setPhaseFlash(false), 5000);
      return () => clearTimeout(t);
    }
    if (!phase) prevPhaseRef.current = null;
  }, [phase]);

  return (
    <div className="app">
      <header className="topbar">
        <div className="tb-side tb-left">
          <span className="tb-mark" aria-hidden="true">
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="#f5a623" strokeWidth="2" strokeLinecap="round">
              <circle cx="12" cy="12" r="4.2" fill="#f5a623" stroke="none" />
              <path d="M12 2.5v2.2M12 19.3v2.2M21.5 12h-2.2M4.7 12H2.5M18.7 5.3l-1.6 1.6M6.9 17.1l-1.6 1.6M18.7 18.7l-1.6-1.6M6.9 6.9 5.3 5.3" />
            </svg>
          </span>
          {total > 0 && <span className="tb-count">{total.toLocaleString()} photos</span>}
        </div>

        {total > 0 && (
          <nav className="view-nav tb-tabs">
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
            <button
              className={view === "places" ? "on" : ""}
              onClick={() => setView("places")}
            >
              Places
            </button>
          </nav>
        )}

        <div className="tb-side tb-right">
          <div className="settings">
            <button
              className="tb-gear"
              aria-label="Settings"
              aria-expanded={showSettings}
              onClick={() => {
                setShowSettings((s) => !s);
                setConfirmReset(false);
                setConfirmRemove(null);
              }}
            >
              <svg width="19" height="19" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
                <circle cx="12" cy="12" r="3" />
                <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
              </svg>
            </button>
            {showSettings && (
              <>
                <div
                  className="menu-backdrop"
                  onClick={() => {
                    setShowSettings(false);
                    setConfirmReset(false);
                    setConfirmRemove(null);
                  }}
                />
                <div className="settings-menu">
                  <button
                    className="menu-item"
                    onClick={() => {
                      setShowSettings(false);
                      handleAdd();
                    }}
                    disabled={busy}
                  >
                    <span className="menu-ic">+</span>
                    <span>{scanning ? "Scanning…" : "Add folder"}</span>
                  </button>
                  <button
                    className="menu-item"
                    onClick={() => {
                      setShowSettings(false);
                      handleRescan();
                    }}
                    disabled={busy}
                  >
                    <span className="menu-ic">↻</span>
                    <span className="menu-label">
                      {rescanning ? "Rescanning…" : "Rescan for changes"}
                      <span className="menu-hint">finds new or moved files — usually automatic</span>
                    </span>
                  </button>
                  {roots.length > 0 && (
                    <>
                      <div className="menu-sep" />
                      <div className="menu-head">Folders</div>
                      {roots.map((r) =>
                        confirmRemove === r ? (
                          <div className="menu-confirm" key={r}>
                            <span className="menu-confirm-q">
                              Remove this folder from the library? The photos on disk are
                              untouched, but their index (and any face work in them) is dropped.
                            </span>
                            <div className="menu-confirm-row">
                              <button className="menu-danger-btn" onClick={() => handleRemove(r)}>
                                Remove
                              </button>
                              <button
                                className="menu-cancel-btn"
                                onClick={() => setConfirmRemove(null)}
                              >
                                Cancel
                              </button>
                            </div>
                          </div>
                        ) : (
                          <div className="roots-item" key={r}>
                            <span className="roots-path" title={r}>
                              {r}
                            </span>
                            <button
                              className="roots-remove"
                              aria-label={`Remove ${r}`}
                              onClick={() => setConfirmRemove(r)}
                            >
                              ✕
                            </button>
                          </div>
                        ),
                      )}
                    </>
                  )}
                  <div className="menu-sep" />
                  {confirmReset ? (
                    <div className="menu-confirm">
                      <span className="menu-confirm-q">
                        Clear every name and grouping and start people over? Your photos are
                        untouched, and the database is backed up first.
                      </span>
                      <div className="menu-confirm-row">
                        <button className="menu-danger-btn" onClick={handleResetPeople}>
                          Start over
                        </button>
                        <button className="menu-cancel-btn" onClick={() => setConfirmReset(false)}>
                          Cancel
                        </button>
                      </div>
                    </div>
                  ) : (
                    <button className="menu-item menu-danger" onClick={() => setConfirmReset(true)}>
                      <span className="menu-ic">⟲</span>
                      <span className="menu-label">
                        Start people over
                        <span className="menu-hint">clears all names and groups, keeps your photos</span>
                      </span>
                    </button>
                  )}
                </div>
              </>
            )}
          </div>
        </div>
      </header>
      <div className={`hairline${activity ? " active" : ""}`}>
        {activity && (
          <>
            <div
              className="hairline-fill"
              style={{ width: `${Math.round(activity.frac * 100)}%` }}
            />
            <div className={`hairline-tip${phaseFlash ? " show" : ""}`} role="status">
              {activityLabel}
            </div>
          </>
        )}
      </div>

      {total > 0 ? (
        view === "people" ? (
          <People focusClusterId={focusClusterId} onFocusConsumed={handleFocusConsumed} />
        ) : view === "places" ? (
          <Places />
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
          <button className="pick-btn empty-add" onClick={handleAdd} disabled={busy}>
            {scanning ? "Scanning…" : "Add folder"}
          </button>
        </div>
      )}

      {resetNote && (
        <div className={`toast${newPerson ? " second" : ""}`} role="status">
          <div className="toast-body">
            <div className="toast-title">People reset</div>
            <div className="toast-sub toast-path" title={resetNote}>
              Backup saved: {resetNote}
            </div>
          </div>
          <button className="toast-x" aria-label="Dismiss" onClick={() => setResetNote(null)}>
            ✕
          </button>
        </div>
      )}

      {newPerson && (
        <div className="toast" role="status">
          <img
            className="toast-face"
            src={faceCropUrl(newPerson.cover_face_id)}
            alt=""
            draggable={false}
          />
          <div className="toast-body">
            <div className="toast-title">New person found</div>
            <div className="toast-sub">
              {faceEligible > 0 && faceScanned < faceEligible
                ? `${newPerson.count.toLocaleString()} photos so far`
                : `${newPerson.count.toLocaleString()} photos · name them if you like`}
            </div>
          </div>
          <button className="toast-name" onClick={nameNewPerson}>
            Name
          </button>
          <button
            className="toast-x"
            aria-label="Dismiss"
            onClick={() => setNewPerson(null)}
          >
            ✕
          </button>
        </div>
      )}
    </div>
  );
}

export default App;
