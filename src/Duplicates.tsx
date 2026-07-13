// The duplicates review: byte-identical groups, walked one at a time — the
// same "one decision per screen, keyboard-first" shape as face review. The
// keeper is pre-picked (paths that look like originals outrank /Downloads and
// backups); one tap hides the rest. Pure curation: hiding never touches a
// file, and every batch shares one Undo.
//
// The screen works over a SNAPSHOT of the report: resolving a group changes
// what the backend would return, and refetching mid-session would renumber
// "Group i of N" under the user's hands. "Keep all" skips for this session
// only — an unresolved group simply returns next visit.

import { useCallback, useEffect, useMemo, useState } from "react";
import UndoToast from "./UndoToast";
import { fmtBytes } from "./format";
import {
  getDuplicateReport,
  setPhotosHidden,
  thumbUrl,
  type DuplicateGroup,
  type DuplicateReport,
} from "./api";

/// Lower score wins the "keep" pre-pick: copies whose path smells like a
/// re-download or a backup lose to ones living in real photo folders; ties go
/// to the earliest-indexed copy (usually the first one that entered the library).
function keeperScore(path: string): number {
  return /download|copia|copy|backup|duplicate|\(\d+\)/i.test(path) ? 1 : 0;
}
function pickKeeper(group: DuplicateGroup): number {
  const best = [...group.copies].sort(
    (a, b) => keeperScore(a.path) - keeperScore(b.path) || a.id - b.id,
  )[0];
  return best.id;
}

function dirOf(path: string): string {
  const i = path.lastIndexOf("/");
  return i > 0 ? path.slice(0, i + 1) : path;
}
function nameOf(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}

export default function Duplicates({
  onBack,
  onCurationChanged,
}: {
  onBack: () => void;
  onCurationChanged?: () => void;
}) {
  const [report, setReport] = useState<DuplicateReport | null>(null);
  const [idx, setIdx] = useState(0);
  const [keeper, setKeeper] = useState<number | null>(null);
  // Session tallies for the "all caught up" payoff line.
  const [resolved, setResolved] = useState(0);
  const [reclaimed, setReclaimed] = useState(0);
  const [undo, setUndo] = useState<{ ids: number[]; label: string } | null>(null);

  useEffect(() => {
    getDuplicateReport().then(setReport).catch(() => {});
  }, []);

  const group = report?.groups[idx];
  useEffect(() => {
    if (group) setKeeper(pickKeeper(group));
  }, [group]);

  const advance = useCallback(() => setIdx((i) => i + 1), []);

  const hideOthers = useCallback(() => {
    if (!group || keeper == null) return;
    const others = group.copies.filter((c) => c.id !== keeper).map((c) => c.id);
    if (others.length === 0) return;
    setPhotosHidden(others, true)
      .then(() => onCurationChanged?.())
      .catch(() => {});
    setResolved((n) => n + 1);
    setReclaimed((b) => b + group.wasted_bytes);
    setUndo({
      ids: others,
      label: `Hid ${others.length} ${others.length === 1 ? "copy" : "copies"}`,
    });
    advance();
  }, [group, keeper, advance, onCurationChanged]);

  const undoHide = useCallback(() => {
    setUndo((u) => {
      if (u) {
        setPhotosHidden(u.ids, false)
          .then(() => onCurationChanged?.())
          .catch(() => {});
        setResolved((n) => Math.max(0, n - 1));
        setIdx((i) => Math.max(0, i - 1));
      }
      return null;
    });
  }, [onCurationChanged]);

  // Y hides, → skips, Esc leaves — the review-session grammar.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onBack();
      else if (e.key.toLowerCase() === "y") hideOthers();
      else if (e.key === "ArrowRight") advance();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onBack, hideOthers, advance]);

  const totalWasted = useMemo(
    () => (report ? report.groups.reduce((n, g) => n + g.wasted_bytes, 0) : 0),
    [report],
  );
  const sweeping = report != null && report.scanned < report.eligible;

  return (
    <div className="curated-view">
      <div className="curated-head">
        <button className="ghost-btn" onClick={onBack}>
          ‹ Back
        </button>
        <span className="curated-title">
          Duplicates
          {report && report.groups.length > 0 && idx < report.groups.length && (
            <> · group {(idx + 1).toLocaleString()} of {report.groups.length.toLocaleString()}</>
          )}
        </span>
        {report && report.groups.length > 0 && (
          <span className="curated-hint">
            {fmtBytes(totalWasted)} of repeats · hiding a copy never touches the file
          </span>
        )}
      </div>

      {sweeping && (
        <div className="dup-sweep">
          Still comparing your library — {report!.scanned.toLocaleString()} of{" "}
          {report!.eligible.toLocaleString()} photos checked; more groups may appear.
        </div>
      )}

      {report == null ? null : group ? (
        <div className="dup-stage">
          <p className="dup-q">
            {group.copies.length} identical copies · {fmtBytes(group.wasted_bytes)} wasted —
            which one stays?
          </p>
          <div className="dup-twins">
            {group.copies.map((c) => (
              <button
                key={c.id}
                className={`dup-twin${keeper === c.id ? " keep" : ""}`}
                onClick={() => setKeeper(c.id)}
                title={c.path}
              >
                <img src={thumbUrl(c.id)} alt="" draggable={false} />
                <span className="dt-name">{nameOf(c.path)}</span>
                <span className="dt-dir">{dirOf(c.path)}</span>
                <span className={`dt-tag${keeper === c.id ? "" : " ghost"}`}>
                  {keeper === c.id ? "Keep" : "Will hide"}
                </span>
              </button>
            ))}
          </div>
          <div className="dup-actions">
            <button className="sb-btn" onClick={hideOthers}>
              Hide {group.copies.length - 1}{" "}
              {group.copies.length - 1 === 1 ? "copy" : "copies"} <span className="rf-key">Y</span>
            </button>
            <button className="sb-btn ghost" onClick={advance}>
              Keep all — skip <span className="rf-key">→</span>
            </button>
          </div>
        </div>
      ) : report.groups.length === 0 && !sweeping ? (
        <div className="empty">
          <p>No exact duplicates.</p>
          <p className="muted">
            Every visible photo in your library is one of a kind, byte for byte.
          </p>
        </div>
      ) : idx > 0 ? (
        <div className="empty">
          <p>All caught up.</p>
          <p className="muted">
            {resolved.toLocaleString()} {resolved === 1 ? "group" : "groups"} resolved ·{" "}
            {fmtBytes(reclaimed)} of repeats hidden from view. Skipped groups return next
            visit.
          </p>
          <button className="ghost-btn" onClick={onBack}>
            ‹ Back to Home
          </button>
        </div>
      ) : null}

      {undo && <UndoToast label={undo.label} onUndo={undoHide} />}
    </div>
  );
}
