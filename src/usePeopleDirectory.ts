// The people list AND the clustering generation its ids belong to, kept fresh
// across background re-clusters — the pattern every correction surface needs:
// a re-cluster renumbers positive cluster ids, so a surface that stays open
// (the viewer, a person page) must refresh both when one finishes, and pass
// `genRef.current` into every cluster-targeting mutation so a stale id is
// refused instead of acting on whatever cluster now holds it.

import { useCallback, useEffect, useRef, useState } from "react";
import { getClusterGeneration, getClusters, onClusterProgress, type Cluster } from "./api";

export function usePeopleDirectory(onSettled?: () => void) {
  const [people, setPeople] = useState<Cluster[]>([]);
  const genRef = useRef(0);
  // Read through a ref so the subscription never re-binds on a new closure.
  const onSettledRef = useRef(onSettled);
  onSettledRef.current = onSettled;

  const refresh = useCallback(() => {
    getClusters().then(setPeople).catch(() => {});
    getClusterGeneration()
      .then((g) => {
        genRef.current = g;
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    refresh();
    let unlisten: (() => void) | undefined;
    onClusterProgress((p) => {
      if (!p.running) {
        refresh();
        onSettledRef.current?.();
      }
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, [refresh]);

  return { people, genRef, refresh };
}
