// Focus review: the training session as an inbox, one decision per screen.
//
// The queue (see build_review_queue in lib.rs) is a payoff-sorted snapshot of every
// suggestion engine's output, normalized to one grammar: yes / no / who. The
// snapshot is captured once on open — actions schedule background re-clusters, and
// a live-updating list would reorder under the user's hands mid-answer. The
// generation guard keeps the snapshot safe: if clustering moves on (a pass
// completes mid-session), the next answer is refused server-side and the session
// ends with a "refreshed" note instead of acting on renumbered clusters.
//
// Keyboard-first: Y (yes / merge all), N (no / not the same), S (someone else…),
// → (skip), Esc (close). Each answer advances; the tally makes progress felt.

import { useCallback, useEffect, useMemo, useState } from "react";
import {
  absorbClusters,
  faceCropUrl,
  getClusters,
  mergeClusters,
  nameCluster,
  notThisPerson,
  rejectMerge,
  type Cluster,
  type ReviewItem,
  type ReviewQueue,
} from "./api";

export default function ReviewFocus({
  queue,
  onClose,
}: {
  queue: ReviewQueue;
  onClose: () => void; // caller reloads People on close
}) {
  // Snapshot the items once — the live queue refreshes behind our back.
  const [items] = useState<ReviewItem[]>(queue.items);
  const generation = queue.generation;
  const [idx, setIdx] = useState(0);
  const [answered, setAnswered] = useState(0);
  const [settled, setSettled] = useState(0); // photos settled this session
  // Strong-batch chips already acted on (by cluster id), for the current item.
  const [chipDone, setChipDone] = useState<Set<number>>(new Set());
  // "Someone else…" picker state.
  const [picking, setPicking] = useState(false);
  const [pickQuery, setPickQuery] = useState("");
  const [people, setPeople] = useState<Cluster[]>([]);
  // Set when an action was refused (clustering moved on) — end the session gently.
  const [stale, setStale] = useState(false);

  const item = idx < items.length ? items[idx] : null;

  useEffect(() => {
    getClusters().then(setPeople).catch(() => {});
  }, []);

  const advance = useCallback(() => {
    setChipDone(new Set());
    setPicking(false);
    setPickQuery("");
    setIdx((i) => i + 1);
  }, []);

  // Run one answer: count it, advance on success, end the session on refusal.
  const act = useCallback(
    (run: () => Promise<unknown>, photos: number) => {
      run()
        .then(() => {
          setAnswered((a) => a + 1);
          setSettled((s) => s + photos);
          advance();
        })
        .catch(() => setStale(true));
    },
    [advance],
  );

  // The named people the "someone else…" picker offers (excluding the proposed one).
  const pickMatches = useMemo(() => {
    const q = pickQuery.trim().toLowerCase();
    const excluded =
      item && (item.kind === "maybe" || item.kind === "strong_batch") ? item.into : null;
    return people
      .filter((c) => c.name && c.cluster_id !== excluded)
      .filter((c) => (q ? c.name!.toLowerCase().includes(q) : true))
      .slice(0, 6);
  }, [people, pickQuery, item]);

  // The cluster under review that "someone else…" reassigns (single-group kinds).
  const pickTargetCluster =
    item?.kind === "maybe" ? item.group.cluster_id : item?.kind === "who_is_this" ? item.cluster_id : null;
  const pickPhotos = item?.kind === "maybe" || item?.kind === "who_is_this" ? item.photos : 0;

  const pickPerson = (target: Cluster) => {
    if (pickTargetCluster == null) return;
    act(() => absorbClusters(target.cluster_id, [pickTargetCluster], generation), pickPhotos);
  };
  const pickNewPerson = (name: string) => {
    if (pickTargetCluster == null || !name.trim()) return;
    // Naming the cluster mints the person directly (and schedules the re-cluster).
    act(() => nameCluster(pickTargetCluster, name.trim()), pickPhotos);
  };

  // Keyboard shortcuts — disabled while the picker's text field is active.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        if (picking) setPicking(false);
        else onClose();
        return;
      }
      if (picking || !item || stale) return;
      const k = e.key.toLowerCase();
      if (k === "arrowright") advance();
      if (item.kind === "maybe") {
        if (k === "y") act(() => absorbClusters(item.into, [item.group.cluster_id], generation), item.photos);
        else if (k === "n") act(() => notThisPerson(item.into, item.group.cluster_id, generation), item.photos);
        else if (k === "s") setPicking(true);
      } else if (item.kind === "pairwise") {
        if (k === "y") act(() => mergeClusters(item.into, item.from, generation), item.photos);
        else if (k === "n") act(() => rejectMerge(item.into, item.from, generation), item.photos);
      } else if (item.kind === "strong_batch") {
        if (k === "y") {
          const rest = item.groups.filter((g) => !chipDone.has(g.cluster_id));
          act(
            () => absorbClusters(item.into, rest.map((g) => g.cluster_id), generation),
            rest.reduce((n, g) => n + g.photos, 0),
          );
        }
      } else if (item.kind === "who_is_this") {
        if (k === "s") setPicking(true);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [item, picking, stale, chipDone, generation, act, advance, onClose]);

  // One strong-batch chip answered: run it, tally it, and advance when the batch
  // empties. Chip actions don't use `act` — the card stays put for the next chip.
  const chipAct = (run: () => Promise<unknown>, clusterId: number, photos: number, total: number) => {
    run()
      .then(() => {
        setAnswered((a) => a + 1);
        setSettled((s) => s + photos);
        setChipDone((d) => {
          const next = new Set(d).add(clusterId);
          if (next.size >= total) advance();
          return next;
        });
      })
      .catch(() => setStale(true));
  };

  const keyHint = (k: string) => <span className="rf-key">{k}</span>;

  const body = () => {
    if (stale) {
      return (
        <div className="rf-done">
          <p className="rf-q">People were just reorganized</p>
          <p className="rf-sub">
            The remaining suggestions were recomputed — close and reopen Review to continue
            with fresh ones. Everything you answered so far was saved.
          </p>
          <div className="rf-actions">
            <button className="sb-btn" onClick={onClose}>Close</button>
          </div>
        </div>
      );
    }
    if (!item) {
      return (
        <div className="rf-done">
          <p className="rf-q">All caught up</p>
          <p className="rf-sub">
            {answered.toLocaleString()} {answered === 1 ? "answer" : "answers"} ·{" "}
            {settled.toLocaleString()} {settled === 1 ? "photo" : "photos"} settled this session.
          </p>
          <div className="rf-actions">
            <button className="sb-btn" onClick={onClose}>Done</button>
          </div>
        </div>
      );
    }
    if (item.kind === "maybe") {
      return (
        <>
          <div className="rf-faces">
            {item.group.face_id != null && (
              <img className="rf-face" src={faceCropUrl(item.group.face_id)} alt="" draggable={false} />
            )}
          </div>
          <p className="rf-q">
            Is this {item.name}? <span className="rf-anchor-strip">{item.anchor_faces.slice(0, 3).map((f) => (
              <img key={f} className="rf-face tiny" src={faceCropUrl(f)} alt="" draggable={false} />
            ))}</span>
          </p>
          <p className="rf-sub">{item.photos.toLocaleString()} {item.photos === 1 ? "photo" : "photos"}</p>
          {picking ? (
            renderPicker()
          ) : (
            <div className="rf-actions">
              <button className="sb-btn" onClick={() => act(() => absorbClusters(item.into, [item.group.cluster_id], generation), item.photos)}>
                Yes {keyHint("Y")}
              </button>
              <button className="sb-btn" onClick={() => act(() => notThisPerson(item.into, item.group.cluster_id, generation), item.photos)}>
                No {keyHint("N")}
              </button>
              <button className="sb-btn" onClick={() => setPicking(true)}>Someone else… {keyHint("S")}</button>
              <button className="sb-btn ghost" onClick={advance}>Skip {keyHint("→")}</button>
            </div>
          )}
        </>
      );
    }
    if (item.kind === "who_is_this") {
      return (
        <>
          <div className="rf-who">
            {item.candidates.slice(0, 2).map((c) => (
              <div className="rf-who-col" key={c.identity_id}>
                <p className="rf-who-name">{c.name}</p>
                <div className="rf-faces">
                  {c.anchor_faces.map((f) => (
                    <img key={f} className="rf-face small" src={faceCropUrl(f)} alt="" draggable={false} />
                  ))}
                </div>
              </div>
            ))}
          </div>
          <div className="rf-faces">
            {item.group_faces.map((f) => (
              <img key={f} className="rf-face mystery" src={faceCropUrl(f)} alt="" draggable={false} />
            ))}
          </div>
          <p className="rf-q">Who is this?</p>
          <p className="rf-sub">
            {item.photos.toLocaleString()} {item.photos === 1 ? "photo" : "photos"} — both match; you decide
          </p>
          {picking ? (
            renderPicker()
          ) : (
            <div className="rf-actions">
              {item.candidates.slice(0, 3).map((c) => (
                <button
                  key={c.identity_id}
                  className="sb-btn"
                  onClick={() => act(() => absorbClusters(c.into, [item.cluster_id], generation), item.photos)}
                >
                  {c.name}
                </button>
              ))}
              <button className="sb-btn" onClick={() => setPicking(true)}>Someone else… {keyHint("S")}</button>
              <button className="sb-btn ghost" onClick={advance}>Not sure {keyHint("→")}</button>
            </div>
          )}
        </>
      );
    }
    if (item.kind === "pairwise") {
      return (
        <>
          <div className="rf-faces">
            {item.into_faces.slice(0, 3).map((f) => (
              <img key={f} className="rf-face small" src={faceCropUrl(f)} alt="" draggable={false} />
            ))}
            <span className="rf-plus">+</span>
            {item.from_faces.slice(0, 3).map((f) => (
              <img key={f} className="rf-face small" src={faceCropUrl(f)} alt="" draggable={false} />
            ))}
          </div>
          <p className="rf-q">Same person{item.into_name ? ` — ${item.into_name}` : ""}?</p>
          <p className="rf-sub">{item.photos.toLocaleString()} {item.photos === 1 ? "photo" : "photos"} would fold in</p>
          <div className="rf-actions">
            <button className="sb-btn" onClick={() => act(() => mergeClusters(item.into, item.from, generation), item.photos)}>
              Merge {keyHint("Y")}
            </button>
            <button className="sb-btn" onClick={() => act(() => rejectMerge(item.into, item.from, generation), item.photos)}>
              Not the same {keyHint("N")}
            </button>
            <button className="sb-btn ghost" onClick={advance}>Skip {keyHint("→")}</button>
          </div>
        </>
      );
    }
    // strong_batch
    const remaining = item.groups.filter((g) => !chipDone.has(g.cluster_id));
    return (
      <>
        <p className="rf-q">
          {remaining.length.toLocaleString()} {remaining.length === 1 ? "group" : "groups"} strongly match{" "}
          {item.name}
          <span className="rf-anchor-strip">
            {item.anchor_faces.slice(0, 3).map((f) => (
              <img key={f} className="rf-face tiny" src={faceCropUrl(f)} alt="" draggable={false} />
            ))}
          </span>
        </p>
        <p className="rf-sub">confirm each, or merge them all</p>
        <div className="rf-chiprow">
          {remaining.map((g) => (
            <div className="pr-chip" key={g.cluster_id}>
              {g.face_id != null ? (
                <img className="pr-face" src={faceCropUrl(g.face_id)} alt="" draggable={false} />
              ) : (
                <div className="pr-face pr-face-blank" />
              )}
              <div className="pr-count">{g.photos.toLocaleString()}</div>
              <div className="pr-yn">
                <button
                  className="pr-y"
                  title={`Yes — this is ${item.name}`}
                  onClick={() =>
                    chipAct(
                      () => absorbClusters(item.into, [g.cluster_id], generation),
                      g.cluster_id,
                      g.photos,
                      item.groups.length,
                    )
                  }
                >
                  ✓
                </button>
                <button
                  className="pr-n"
                  title="Not this person"
                  onClick={() =>
                    chipAct(
                      () => notThisPerson(item.into, g.cluster_id, generation),
                      g.cluster_id,
                      g.photos,
                      item.groups.length,
                    )
                  }
                >
                  ✕
                </button>
              </div>
            </div>
          ))}
        </div>
        <div className="rf-actions">
          <button
            className="sb-btn"
            onClick={() =>
              act(
                () => absorbClusters(item.into, remaining.map((g) => g.cluster_id), generation),
                remaining.reduce((n, g) => n + g.photos, 0),
              )
            }
          >
            Merge {remaining.length === item.groups.length ? "all" : "remaining"}{" "}
            {remaining.length.toLocaleString()} {keyHint("Y")}
          </button>
          <button className="sb-btn ghost" onClick={advance}>Skip {keyHint("→")}</button>
        </div>
      </>
    );
  };

  function renderPicker() {
    return (
      <div className="sb-picker rf-picker">
        <input
          className="pname-input"
          autoFocus
          value={pickQuery}
          placeholder="Who is it?"
          onChange={(e) => setPickQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") setPicking(false);
            else if (e.key === "Enter" && pickQuery.trim()) pickNewPerson(pickQuery.trim());
          }}
        />
        <ul className="sb-matches">
          {pickMatches.map((m) => (
            <li key={m.cluster_id} className="sb-match" onClick={() => pickPerson(m)}>
              <img className="ns-face" src={faceCropUrl(m.cover_face_id)} alt="" draggable={false} />
              <span className="ns-name">{m.name}</span>
              <span className="ns-count">{m.count.toLocaleString()}</span>
            </li>
          ))}
          {pickQuery.trim() && (
            <li className="sb-match sb-new" onClick={() => pickNewPerson(pickQuery.trim())}>
              + New person “{pickQuery.trim()}”
            </li>
          )}
        </ul>
      </div>
    );
  }

  return (
    <div className="rf-overlay" onClick={onClose}>
      <div className="rf-card" onClick={(e) => e.stopPropagation()}>
        <div className="rf-progress">
          <div className="rf-bar">
            <div
              className="rf-fill"
              style={{ width: `${Math.round((Math.min(idx, items.length) / Math.max(items.length, 1)) * 100)}%` }}
            />
          </div>
          <span className="rf-count">
            {Math.min(idx + 1, items.length)} of {items.length}
          </span>
          <button className="rf-x" aria-label="Close" title="Close (Esc)" onClick={onClose}>
            ✕
          </button>
        </div>
        {body()}
        {settled > 0 && !stale && item && (
          <p className="rf-tally">{settled.toLocaleString()} photos settled this session</p>
        )}
      </div>
    </div>
  );
}
