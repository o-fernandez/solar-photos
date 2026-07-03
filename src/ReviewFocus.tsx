// Focus review: the training session as an inbox, one decision per screen.
//
// The queue (see build_review_queue in lib.rs) is a payoff-sorted snapshot of every
// suggestion engine's output, normalized to one grammar: yes / no / who. The
// snapshot is captured on open — actions schedule background re-clusters, and a
// live-updating list would reorder under the user's hands mid-answer. The
// generation guard keeps the snapshot safe: if clustering moves on (a pass
// completes mid-session — a correction landed, or the sweep found new photos),
// the next answer is refused server-side and the session refetches a fresh queue
// and CONTINUES, instead of dead-ending. Answers already given were saved.
//
// Evidence grammar, shared across card kinds: the group IN QUESTION is labeled and
// amber-ringed; the reference person's confirmed faces sit in their own labeled
// panel. One glance says which faces you're judging and which you're judging
// against — the old cards interleaved them and "Is this X?" was ambiguous.
//
// Every answer is undoable: cluster-level actions return a CorrectionUndo token
// (prior face states + any links added), and the last answer stays revertable via
// an inline line until the next one replaces it. "No" writes a durable
// cannot-link, so a misclick needs a way back.
//
// Keyboard-first: Y (yes / merge all), N (no / not the same), S (someone else…),
// M (it's a mix…), → (skip), Esc (close). Each answer advances; the tally makes
// progress felt.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  absorbClusters,
  confirmFacesIntoCluster,
  detachFaces,
  faceCropUrl,
  getClusterFaces,
  getClusters,
  getReviewQueue,
  mergeClusters,
  nameCluster,
  notThisPerson,
  photoUrl,
  rejectMerge,
  resolveSamePhoto,
  setReviewActive,
  undoCorrection,
  type Cluster,
  type CorrectionUndo,
  type ReviewItem,
  type ReviewQueue,
} from "./api";
import { usePickerNav } from "./pickerNav";

/** Reverses one answer on the backend. */
type Revert = () => Promise<unknown>;

/** The last answer given, kept revertable until the next one replaces it: the
 *  backend revert plus the UI snapshot that re-shows the undone card. */
interface LastAnswer {
  label: string;
  revert: Revert;
  idx: number;
  chipDone: Set<number>;
  answered: number;
  settled: number;
}

/** Chain several undo tokens into one revert (applied in reverse order). */
function revertAll(toks: CorrectionUndo[]): Revert {
  return async () => {
    for (const tok of [...toks].reverse()) await undoCorrection(tok);
  };
}

function photosLabel(n: number): string {
  return `${n.toLocaleString()} ${n === 1 ? "photo" : "photos"}`;
}

export default function ReviewFocus({
  queue,
  onClose,
}: {
  queue: ReviewQueue;
  onClose: () => void; // caller reloads People on close
}) {
  // Snapshot of the queue; replaced wholesale by refresh() when clustering moves on.
  const [items, setItems] = useState<ReviewItem[]>(queue.items);
  const [generation, setGeneration] = useState(queue.generation);
  const [idx, setIdx] = useState(0);
  const [answered, setAnswered] = useState(0);
  const [settled, setSettled] = useState(0); // photos settled this session
  // Strong-batch / twin-pair chips already acted on, for the current item.
  const [chipDone, setChipDone] = useState<Set<number>>(new Set());
  // "Someone else…" picker state.
  const [picking, setPicking] = useState(false);
  const [pickQuery, setPickQuery] = useState("");
  const [people, setPeople] = useState<Cluster[]>([]);
  // "It's a mix…" split state (maybe + who_is_this): the contested cluster's full
  // face set, and each tagged face's slot — 0/1 = a candidate person, 2 = "someone
  // else" (detach and let each face re-home by appearance).
  const [splitting, setSplitting] = useState(false);
  const [splitFaces, setSplitFaces] = useState<number[]>([]);
  const [splitLoading, setSplitLoading] = useState(false);
  const [tags, setTags] = useState<Map<number, 0 | 1 | 2>>(new Map());
  // Sample faces of the group under review on a "maybe" card — judging a whole
  // group from its single cover face was guesswork.
  const [groupSamples, setGroupSamples] = useState<number[]>([]);
  // True while we're waiting out a re-cluster and refetching the queue.
  const [refreshing, setRefreshing] = useState(false);
  // Transient "we refreshed under you" note after a mid-session reorganization.
  const [note, setNote] = useState<string | null>(null);
  // The last answer, revertable inline until the next answer replaces it.
  const [lastAnswer, setLastAnswer] = useState<LastAnswer | null>(null);
  const refreshTries = useRef(0);

  const item = !refreshing && idx < items.length ? items[idx] : null;

  useEffect(() => {
    getClusters().then(setPeople).catch(() => {});
  }, []);

  // Hold the debounced self-heal pass while the session is open: answers apply
  // instantly, but the re-cluster (which renumbers the remaining cards' ids)
  // waits until close. Ending the session (unmount) releases any deferred pass.
  useEffect(() => {
    setReviewActive(true).catch(() => {});
    return () => {
      setReviewActive(false).catch(() => {});
    };
  }, []);

  // The maybe card's group samples: best faces of the group under review (the
  // single cover face is the instant fallback while they load).
  useEffect(() => {
    if (item?.kind !== "maybe") {
      setGroupSamples([]);
      return;
    }
    let alive = true;
    setGroupSamples(item.group.face_id != null ? [item.group.face_id] : []);
    getClusterFaces(item.group.cluster_id)
      .then((f) => {
        if (alive && f.length > 0) setGroupSamples(f.slice(0, 4));
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [item]);

  const advance = useCallback(() => {
    setChipDone(new Set());
    setPicking(false);
    setPickQuery("");
    setSplitting(false);
    setSplitFaces([]);
    setTags(new Map());
    setIdx((i) => i + 1);
  }, []);

  // Clustering moved on mid-session (an answer was refused, or a pass completed):
  // refetch the queue and continue with fresh items. While a pass is still
  // running the queue reads empty — poll until it lands rather than giving up.
  // The last answer's UI snapshot is meaningless against fresh items, so it drops.
  const refresh = useCallback(() => {
    setRefreshing(true);
    setLastAnswer(null);
    const attempt = () => {
      getReviewQueue()
        .then((q) => {
          if (q.items.length === 0 && refreshTries.current < 10) {
            refreshTries.current += 1;
            window.setTimeout(attempt, 1500);
            return;
          }
          refreshTries.current = 0;
          setItems(q.items);
          setGeneration(q.generation);
          setIdx(0);
          setChipDone(new Set());
          setPicking(false);
          setPickQuery("");
          setSplitting(false);
          setSplitFaces([]);
          setTags(new Map());
          setRefreshing(false);
          setNote("People were reorganized — continuing with fresh suggestions.");
          window.setTimeout(() => setNote(null), 4000);
        })
        .catch(() => onClose());
    };
    attempt();
  }, [onClose]);

  // A stale-generation refusal means clustering moved on — refetch and continue.
  // Any OTHER backend refusal (e.g. "that group already belongs to another named
  // person") used to be swallowed into the same "reorganized" flow, which read as
  // a lie and re-showed the unanswerable card; now it's said out loud and the
  // card is skipped.
  const flashNote = useCallback((msg: string) => {
    setNote(msg);
    window.setTimeout(() => setNote(null), 4000);
  }, []);
  const handleRefusal = useCallback(
    (e: unknown, skip: () => void) => {
      const msg = String(e ?? "");
      if (/stale|reorganiz/i.test(msg)) {
        refresh();
      } else {
        flashNote(msg || "Couldn't apply that.");
        skip();
      }
    },
    [refresh, flashNote],
  );

  // The pre-answer UI snapshot `act`/`chipAct` store for undo, read through a ref
  // so their callbacks don't re-create (and re-bind the key listener) per answer.
  const snapRef = useRef({ idx, chipDone, answered, settled });
  snapRef.current = { idx, chipDone, answered, settled };

  // Run one answer: count it, remember how to take it back, advance on success,
  // refresh-and-continue on refusal.
  const act = useCallback(
    (run: () => Promise<Revert>, photos: number, label: string) => {
      const snap = { ...snapRef.current };
      run()
        .then((revert) => {
          setAnswered((a) => a + 1);
          setSettled((s) => s + photos);
          setLastAnswer({ label, revert, ...snap });
          advance();
        })
        .catch((e) => handleRefusal(e, advance));
    },
    [advance, handleRefusal],
  );

  // Take back the last answer: revert on the backend, then restore the UI to the
  // moment before it — the undone card comes back, ready to answer again.
  const doUndo = useCallback(() => {
    const la = lastAnswer;
    if (!la) return;
    setLastAnswer(null);
    la.revert()
      .then(() => {
        setIdx(la.idx);
        setChipDone(new Set(la.chipDone));
        setAnswered(la.answered);
        setSettled(la.settled);
        setPicking(false);
        setPickQuery("");
        setSplitting(false);
        setSplitFaces([]);
        setTags(new Map());
      })
      .catch(() => flashNote("Couldn't undo that."));
  }, [lastAnswer, flashNote]);

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
    act(
      async () => {
        const tok = await absorbClusters(target.cluster_id, [pickTargetCluster], generation);
        return () => undoCorrection(tok);
      },
      pickPhotos,
      `Moved ${photosLabel(pickPhotos)} to ${target.name}`,
    );
  };
  const pickNewPerson = (name: string) => {
    if (pickTargetCluster == null || !name.trim()) return;
    const nm = name.trim();
    // Naming the cluster mints the person directly (and schedules the re-cluster);
    // undo renames the (now canonical) group back to unnamed.
    act(
      async () => {
        const g = await nameCluster(pickTargetCluster, nm, generation);
        return () => nameCluster(g, "");
      },
      pickPhotos,
      `Named ${nm}`,
    );
  };

  // ↑/↓ + Enter in the "someone else…" picker: Enter takes the highlighted row —
  // the TOP MATCH by default, so typing "Cami" ⏎ picks Camila instead of minting
  // a new person named "Cami". The "+ New person" row is the last row.
  const pickRowCount = pickMatches.length + (pickQuery.trim() ? 1 : 0);
  const pickNav = usePickerNav(pickRowCount, (i) => {
    if (i < pickMatches.length) pickPerson(pickMatches[i]);
    else pickNewPerson(pickQuery.trim());
  });

  // "It's a mix…": the group genuinely holds more than one person. Load its full
  // face set so the user can tag each face — a candidate person, or "someone else"
  // — instead of being forced to a single whole-group verdict (or skipping forever).
  // The candidates: who_is_this offers its two, maybe offers its one proposed person.
  const splitCandidates: { name: string; into: number }[] =
    item?.kind === "who_is_this"
      ? item.candidates.slice(0, 2).map((c) => ({ name: c.name, into: c.into }))
      : item?.kind === "maybe"
        ? [{ name: item.name, into: item.into }]
        : [];
  const splitClusterId =
    item?.kind === "who_is_this" ? item.cluster_id : item?.kind === "maybe" ? item.group.cluster_id : null;
  const beginSplit = () => {
    if (splitClusterId == null) return;
    setSplitLoading(true);
    getClusterFaces(splitClusterId)
      .then((f) => {
        setSplitFaces(f);
        setTags(new Map());
        setSplitting(true);
      })
      .catch(() => {})
      .finally(() => setSplitLoading(false));
  };
  const endSplit = () => {
    setSplitting(false);
    setTags(new Map());
  };
  // Tap a face to cycle its tag through the candidates, "someone else", then clear.
  const cycleTag = (faceId: number) =>
    setTags((prev) => {
      const next = new Map(prev);
      const cur = next.get(faceId);
      if (cur === undefined) next.set(faceId, 0);
      else if (cur === 0 && splitCandidates.length > 1) next.set(faceId, 1);
      else if (cur !== 2) next.set(faceId, 2);
      else next.delete(faceId);
      return next;
    });
  // Confirm each candidate batch into its person; "someone else" faces detach and
  // re-home by appearance (they may be one new person, several, or none — so we don't
  // force them together). Untagged faces stay put (partial splits are fine — the card
  // returns for whatever's left). One `act` so it counts as one answer and advances.
  const applySplit = () => {
    if (!item || splitCandidates.length === 0) return;
    const buckets: number[][] = [[], [], []];
    for (const [fid, t] of tags) buckets[t].push(fid);
    const total = buckets[0].length + buckets[1].length + buckets[2].length;
    if (total === 0) return;
    act(
      async () => {
        const toks: CorrectionUndo[] = [];
        for (let i = 0; i < splitCandidates.length; i++) {
          if (buckets[i].length)
            toks.push(await confirmFacesIntoCluster(buckets[i], splitCandidates[i].into, generation));
        }
        if (buckets[2].length) toks.push(await detachFaces(buckets[2]));
        return revertAll(toks);
      },
      total,
      `Split ${total.toLocaleString()} ${total === 1 ? "face" : "faces"}`,
    );
  };

  // Keyboard shortcuts — disabled while the picker's text field or split grid is up.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        if (picking) setPicking(false);
        else if (splitting) endSplit();
        else onClose();
        return;
      }
      if (picking || splitting || !item) return;
      const k = e.key.toLowerCase();
      if (k === "arrowright") advance();
      if (item.kind === "maybe") {
        if (k === "y")
          act(
            async () => {
              const tok = await absorbClusters(item.into, [item.group.cluster_id], generation);
              return () => undoCorrection(tok);
            },
            item.photos,
            `Added ${photosLabel(item.photos)} to ${item.name}`,
          );
        else if (k === "n")
          act(
            async () => {
              const tok = await notThisPerson(item.into, item.group.cluster_id, generation);
              return () => undoCorrection(tok);
            },
            item.photos,
            `Marked not ${item.name}`,
          );
        else if (k === "s") setPicking(true);
        else if (k === "m") beginSplit();
      } else if (item.kind === "pairwise") {
        if (k === "y")
          act(
            async () => {
              const tok = await mergeClusters(item.into, item.from, generation);
              return () => undoCorrection(tok);
            },
            item.photos,
            item.into_name ? `Merged into ${item.into_name}` : "Merged — same person",
          );
        else if (k === "n")
          act(
            async () => {
              const tok = await rejectMerge(item.into, item.from, generation);
              return () => undoCorrection(tok);
            },
            item.photos,
            "Kept apart — not the same",
          );
      } else if (item.kind === "same_photo_twin") {
        const rest = item.pairs.filter((p) => !chipDone.has(p.from));
        if (k === "y" || k === "n") {
          act(
            async () => {
              const toks: CorrectionUndo[] = [];
              for (const p of rest)
                toks.push(await resolveSamePhoto(p.into, p.from, k === "y", generation));
              return revertAll(toks);
            },
            rest.reduce((n, p) => n + p.photos, 0),
            k === "y" ? "Same person (collage/mirror)" : "Kept apart — two people",
          );
        }
      } else if (item.kind === "strong_batch") {
        if (k === "y") {
          const rest = item.groups.filter((g) => !chipDone.has(g.cluster_id));
          act(
            async () => {
              const tok = await absorbClusters(item.into, rest.map((g) => g.cluster_id), generation);
              return () => undoCorrection(tok);
            },
            rest.reduce((n, g) => n + g.photos, 0),
            `Merged ${rest.length.toLocaleString()} ${rest.length === 1 ? "group" : "groups"} into ${item.name}`,
          );
        }
      } else if (item.kind === "who_is_this") {
        if (k === "s") setPicking(true);
        else if (k === "m") beginSplit();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [item, picking, splitting, chipDone, generation, act, advance, onClose]);

  // One strong-batch / twin-pair chip answered: run it, tally it, remember the
  // take-back, and advance when the card empties. Chip actions don't use `act` —
  // the card stays for the rest. A non-stale refusal hides the chip WITHOUT
  // counting it: the backend says the question can't be answered as asked, so
  // re-offering it forever helps no one.
  const chipAct = (
    run: () => Promise<Revert>,
    chipId: number,
    photos: number,
    total: number,
    label: string,
  ) => {
    const snap = { ...snapRef.current };
    const hideChip = () =>
      setChipDone((d) => {
        const next = new Set(d).add(chipId);
        if (next.size >= total) advance();
        return next;
      });
    run()
      .then((revert) => {
        setAnswered((a) => a + 1);
        setSettled((s) => s + photos);
        setLastAnswer({ label, revert, ...snap });
        hideChip();
      })
      .catch((e) => handleRefusal(e, hideChip));
  };

  const keyHint = (k: string) => <span className="rf-key">{k}</span>;

  const face = (f: number, cls = "") => (
    <img key={f} className={`rf-face ${cls}`.trim()} src={faceCropUrl(f)} alt="" draggable={false} />
  );

  // The two-panel evidence block: the group in question (amber-ringed) on the
  // left, the reference on the right — both labeled.
  const panels = (
    leftLabel: string,
    leftFaces: number[],
    rightLabel: string,
    rightFaces: number[],
    ringLeft: boolean,
  ) => (
    <div className="rf-panels">
      <div className="rf-panel">
        <span className="rf-panel-label">{leftLabel}</span>
        <div className="rf-faces">{leftFaces.map((f) => face(f, ringLeft ? "small mystery" : "small"))}</div>
      </div>
      <div className="rf-panel-div" />
      <div className="rf-panel">
        <span className="rf-panel-label">{rightLabel}</span>
        <div className="rf-faces">{rightFaces.map((f) => face(f, "small"))}</div>
      </div>
    </div>
  );

  // The "It's a mix…" tagging grid, shared by maybe (one candidate) and
  // who_is_this (two candidates).
  const renderSplit = () => {
    const counts = [0, 0, 0];
    for (const t of tags.values()) counts[t]++;
    const total = counts[0] + counts[1] + counts[2];
    const slotCls = ["a", "b", "c"];
    return (
      <>
        <p className="rf-q">
          {splitCandidates.length > 1
            ? "Which is which?"
            : `Tag the faces that are ${splitCandidates[0]?.name ?? "them"}`}
        </p>
        <div className="rf-split-legend">
          {splitCandidates.map((c, i) => (
            <span key={c.into} className={`rf-split-key ${slotCls[i]}`}>
              {c.name} · {counts[i].toLocaleString()}
            </span>
          ))}
          <span className="rf-split-key c">Someone else · {counts[2].toLocaleString()}</span>
        </div>
        <p className="rf-sub">
          Tap a face to cycle its tag — untagged faces stay put for later.
        </p>
        <div className="rf-split-grid">
          {splitFaces.map((f) => {
            const t = tags.get(f);
            const cls = t === undefined ? "" : slotCls[t];
            return (
              <button key={f} className={`rf-split-face ${cls}`} onClick={() => cycleTag(f)}>
                {face(f, "small")}
              </button>
            );
          })}
        </div>
        <div className="rf-actions">
          <button className="sb-btn" disabled={total === 0} onClick={applySplit}>
            Apply split ({total.toLocaleString()})
          </button>
          <button className="sb-btn ghost" onClick={endSplit}>Cancel</button>
        </div>
      </>
    );
  };

  const body = () => {
    if (refreshing) {
      return (
        <div className="rf-done">
          <p className="rf-q">People were just reorganized</p>
          <p className="rf-sub">
            Fetching fresh suggestions — everything you answered so far was saved.
          </p>
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
      if (splitting) return renderSplit();
      return (
        <>
          {panels(
            `This group · ${photosLabel(item.photos)}`,
            groupSamples,
            item.name,
            item.anchor_faces.slice(0, 3),
            true,
          )}
          <p className="rf-q">
            {groupSamples.length > 1 ? `Are these ${item.name}?` : `Is this ${item.name}?`}
          </p>
          {picking ? (
            renderPicker()
          ) : (
            <div className="rf-actions">
              <button
                className="sb-btn"
                onClick={() =>
                  act(
                    async () => {
                      const tok = await absorbClusters(item.into, [item.group.cluster_id], generation);
                      return () => undoCorrection(tok);
                    },
                    item.photos,
                    `Added ${photosLabel(item.photos)} to ${item.name}`,
                  )
                }
              >
                Yes {keyHint("Y")}
              </button>
              <button
                className="sb-btn"
                onClick={() =>
                  act(
                    async () => {
                      const tok = await notThisPerson(item.into, item.group.cluster_id, generation);
                      return () => undoCorrection(tok);
                    },
                    item.photos,
                    `Marked not ${item.name}`,
                  )
                }
              >
                No {keyHint("N")}
              </button>
              <button className="sb-btn" onClick={() => setPicking(true)}>
                Someone else… {keyHint("S")}
              </button>
              <button className="sb-btn" disabled={splitLoading} onClick={beginSplit}>
                {splitLoading ? "Loading…" : <>It's a mix… {keyHint("M")}</>}
              </button>
              <button className="sb-btn ghost" onClick={advance}>Skip {keyHint("→")}</button>
            </div>
          )}
        </>
      );
    }
    if (item.kind === "who_is_this") {
      if (splitting) return renderSplit();
      return (
        <>
          <div className="rf-who">
            {item.candidates.slice(0, 2).map((c) => (
              <div className="rf-who-col" key={c.identity_id}>
                <p className="rf-who-name">{c.name}</p>
                <div className="rf-faces">{c.anchor_faces.map((f) => face(f, "small"))}</div>
              </div>
            ))}
          </div>
          <span className="rf-panel-label">This group · {photosLabel(item.photos)}</span>
          <div className="rf-faces">{item.group_faces.map((f) => face(f, "mystery"))}</div>
          <p className="rf-q">Who is this?</p>
          <p className="rf-sub">both match; you decide</p>
          {picking ? (
            renderPicker()
          ) : (
            <div className="rf-actions">
              {item.candidates.slice(0, 3).map((c) => (
                <button
                  key={c.identity_id}
                  className="sb-btn"
                  onClick={() =>
                    act(
                      async () => {
                        const tok = await absorbClusters(c.into, [item.cluster_id], generation);
                        return () => undoCorrection(tok);
                      },
                      item.photos,
                      `Added ${photosLabel(item.photos)} to ${c.name}`,
                    )
                  }
                >
                  {c.name}
                </button>
              ))}
              <button className="sb-btn" disabled={splitLoading} onClick={beginSplit}>
                {splitLoading ? "Loading…" : <>It's a mix… {keyHint("M")}</>}
              </button>
              <button className="sb-btn" onClick={() => setPicking(true)}>
                Someone else… {keyHint("S")}
              </button>
              <button className="sb-btn ghost" onClick={advance}>Not sure {keyHint("→")}</button>
            </div>
          )}
        </>
      );
    }
    if (item.kind === "same_photo_twin") {
      const rest = item.pairs.filter((p) => !chipDone.has(p.from));
      const one = rest.length === 1 && item.pairs.length === 1;
      return (
        <>
          <img className="rf-photo" src={photoUrl(item.photo_id)} alt="" draggable={false} />
          <p className="rf-q">
            {one
              ? "These two are in this one photo — same person?"
              : `${rest.length.toLocaleString()} look-alike pairs in this one photo — same person, each?`}
          </p>
          <p className="rf-sub">
            A collage, mirror, or photo-of-a-photo shows one person twice; twins or
            look-alike siblings are two people.
          </p>
          <div className="rf-chiprow">
            {rest.map((p) => (
              <div className="pr-chip rf-twinpair" key={p.from}>
                <div className="rf-faces">
                  {face(p.face_a, "small")}
                  {face(p.face_b, "small")}
                </div>
                {p.into_name && <div className="pr-count">{p.into_name}?</div>}
                <div className="pr-yn">
                  <button
                    className="pr-y"
                    title={`Same person${p.into_name ? ` — merge into ${p.into_name}` : ""} (collage/mirror)`}
                    onClick={() =>
                      chipAct(
                        async () => {
                          const tok = await resolveSamePhoto(p.into, p.from, true, generation);
                          return () => undoCorrection(tok);
                        },
                        p.from,
                        p.photos,
                        item.pairs.length,
                        "Same person (collage/mirror)",
                      )
                    }
                  >
                    ✓
                  </button>
                  <button
                    className="pr-n"
                    title="Two people — keep them apart for good"
                    onClick={() =>
                      chipAct(
                        async () => {
                          const tok = await resolveSamePhoto(p.into, p.from, false, generation);
                          return () => undoCorrection(tok);
                        },
                        p.from,
                        p.photos,
                        item.pairs.length,
                        "Kept apart — two people",
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
                  async () => {
                    const toks: CorrectionUndo[] = [];
                    for (const p of rest)
                      toks.push(await resolveSamePhoto(p.into, p.from, true, generation));
                    return revertAll(toks);
                  },
                  rest.reduce((n, p) => n + p.photos, 0),
                  "Same person (collage/mirror)",
                )
              }
            >
              {one ? "Same person" : `All same person (${rest.length.toLocaleString()})`} {keyHint("Y")}
            </button>
            <button
              className="sb-btn"
              onClick={() =>
                act(
                  async () => {
                    const toks: CorrectionUndo[] = [];
                    for (const p of rest)
                      toks.push(await resolveSamePhoto(p.into, p.from, false, generation));
                    return revertAll(toks);
                  },
                  rest.reduce((n, p) => n + p.photos, 0),
                  "Kept apart — two people",
                )
              }
            >
              {one ? "Two people" : "All two people"} {keyHint("N")}
            </button>
            <button className="sb-btn ghost" onClick={advance}>Skip {keyHint("→")}</button>
          </div>
        </>
      );
    }
    if (item.kind === "pairwise") {
      const named = item.into_name != null;
      return (
        <>
          {panels(
            named ? `This group · ${photosLabel(item.photos)}` : "Group A",
            item.from_faces.slice(0, 3),
            item.into_name ?? "Group B",
            item.into_faces.slice(0, 3),
            named,
          )}
          <p className="rf-q">Same person{named ? ` — ${item.into_name}` : ""}?</p>
          <p className="rf-sub">{photosLabel(item.photos)} would fold in</p>
          <div className="rf-actions">
            <button
              className="sb-btn"
              onClick={() =>
                act(
                  async () => {
                    const tok = await mergeClusters(item.into, item.from, generation);
                    return () => undoCorrection(tok);
                  },
                  item.photos,
                  named ? `Merged into ${item.into_name}` : "Merged — same person",
                )
              }
            >
              Yes, merge {keyHint("Y")}
            </button>
            <button
              className="sb-btn"
              onClick={() =>
                act(
                  async () => {
                    const tok = await rejectMerge(item.into, item.from, generation);
                    return () => undoCorrection(tok);
                  },
                  item.photos,
                  "Kept apart — not the same",
                )
              }
            >
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
          {remaining.length.toLocaleString()} {remaining.length === 1 ? "group" : "groups"} strongly
          match {item.name}
        </p>
        <div className="rf-refrow">
          <span className="rf-panel-label">{item.name}</span>
          {item.anchor_faces.slice(0, 3).map((f) => face(f, "tiny"))}
        </div>
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
                      async () => {
                        const tok = await absorbClusters(item.into, [g.cluster_id], generation);
                        return () => undoCorrection(tok);
                      },
                      g.cluster_id,
                      g.photos,
                      item.groups.length,
                      `Added ${photosLabel(g.photos)} to ${item.name}`,
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
                      async () => {
                        const tok = await notThisPerson(item.into, g.cluster_id, generation);
                        return () => undoCorrection(tok);
                      },
                      g.cluster_id,
                      g.photos,
                      item.groups.length,
                      `Marked not ${item.name}`,
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
                async () => {
                  const tok = await absorbClusters(
                    item.into,
                    remaining.map((g) => g.cluster_id),
                    generation,
                  );
                  return () => undoCorrection(tok);
                },
                remaining.reduce((n, g) => n + g.photos, 0),
                `Merged ${remaining.length.toLocaleString()} ${remaining.length === 1 ? "group" : "groups"} into ${item.name}`,
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
          onChange={(e) => {
            setPickQuery(e.target.value);
            pickNav.resetHighlight();
          }}
          onKeyDown={(e) => {
            if (e.key === "Escape") setPicking(false);
            else pickNav.onNavKey(e);
          }}
        />
        <ul className="sb-matches">
          {pickMatches.map((m, i) => (
            <li
              key={m.cluster_id}
              className={`sb-match${pickNav.highlight === i ? " hi" : ""}`}
              onMouseEnter={() => pickNav.setHighlight(i)}
              onClick={() => pickPerson(m)}
            >
              <img className="ns-face" src={faceCropUrl(m.cover_face_id)} alt="" draggable={false} />
              <span className="ns-name">{m.name}</span>
              <span className="ns-count">{m.count.toLocaleString()}</span>
            </li>
          ))}
          {pickQuery.trim() && (
            <li
              className={`sb-match sb-new${pickNav.highlight === pickMatches.length ? " hi" : ""}`}
              onMouseEnter={() => pickNav.setHighlight(pickMatches.length)}
              onClick={() => pickNewPerson(pickQuery.trim())}
            >
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
          <span className="rf-count">
            {refreshing
              ? "…"
              : items.length > 0 && idx < items.length
                ? `${(idx + 1).toLocaleString()} of ${items.length.toLocaleString()}`
                : ""}
          </span>
          {(item || refreshing) && (
            <button className="rf-x" aria-label="Close" title="Close (Esc)" onClick={onClose}>
              ✕
            </button>
          )}
        </div>
        {body()}
        {note && <p className="rf-note">{note}</p>}
        {lastAnswer && !refreshing && (
          <p className="rf-last">
            <span className="rf-last-label">{lastAnswer.label}</span>
            <button className="rf-undo" onClick={doUndo}>
              Undo
            </button>
          </p>
        )}
        {settled > 0 && item && (
          <p className="rf-tally">{settled.toLocaleString()} photos settled this session</p>
        )}
      </div>
    </div>
  );
}
